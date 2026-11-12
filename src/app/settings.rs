//! Preferences: the LLM settings modal (open, save), the appearance (theme)
//! preference and its live preview, and the cached chat-model config every AI
//! flow reads through one gate.
//!
//! Its messages, [`SettingsMsg`], arrive through `App::update_settings`.

use crate::app::prelude::*;
use crate::*;

/// Most tokens Test connection lets the model answer with.
/// Enough for a word, and for a model that thinks before it answers to be
/// cut short — an answer all the same.
const CONNECTION_TEST_TOKENS: u32 = 16;

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
        // The saved model changed: a check of the one before is not of it.
        if self.model_check.of.is_some() {
            let now = self.llm_config();
            if now.as_ref().map(crate::app::state::ModelCheck::of_config) != self.model_check.of {
                self.model_check = Default::default();
            }
        }
        // What estimates and the Usage page read prices for, and the prices
        // as they are now (another window may have changed them).
        crate::app::usage::forget_own_prices();
        self.refresh_usage_model();
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
                Err(Task::done(Message::Settings(SettingsMsg::NeedKey)))
            }
        }
    }

    /// Test connection: one word asked of the model the Language model
    /// page's fields name, as they stand — saved or not.
    pub(crate) fn on_test_connection(&mut self) -> Task<Message> {
        let cfg = llm::Config::from_form(
            self.settings.provider,
            self.settings.key.clone(),
            self.settings.model.clone(),
            self.settings.base_url.clone(),
        );
        self.test_connection_of(cfg, false)
    }

    /// Test connection of the model as saved: what every call uses.
    pub(crate) fn on_test_saved_connection(&mut self) -> Task<Message> {
        match self.llm_config() {
            Some(cfg) => {
                self.model_check.of = Some(crate::app::state::ModelCheck::of_config(&cfg));
                self.test_connection_of(cfg, true)
            }
            None => Task::done(Message::Settings(SettingsMsg::NeedKey)),
        }
    }

    /// One word asked of the model `cfg` names: what Test connection finds
    /// — of the form's fields (`SettingsDraft::connection_test`), or
    /// `saved`, of the model as saved (`App::model_check`).
    fn test_connection_of(&mut self, cfg: llm::Config, saved: bool) -> Task<Message> {
        use crate::app::state::ConnectionTest;
        self.settings.connection_tests += 1;
        let number = self.settings.connection_tests;
        let found = if saved {
            &mut self.model_check.result
        } else {
            &mut self.settings.connection_test
        };
        if cfg.base_url.is_empty() || cfg.model.is_empty() {
            *found = ConnectionTest::Failed(crate::app::state::TestFailure::Said(
                tr_noop!("Name the model and its endpoint first.").to_string(),
            ));
            return Task::none();
        }
        // No key, for a service that takes one: said as that, not as the
        // server's "no AI chat config". A custom endpoint may need none.
        if cfg.api_key.trim().is_empty() && cfg.provider != llm::Provider::Custom {
            *found = ConnectionTest::Failed(crate::app::state::TestFailure::Said(
                tr_noop!("Enter the API key first.").to_string(),
            ));
            return Task::none();
        }
        *found = ConnectionTest::Testing(number);
        // Asked the way every call is made: through the server, for a
        // project on another machine.
        let ai = self.ai_client();
        Task::perform(
            async move {
                let model = cfg.model.clone();
                ai.complete_typed(
                    cfg.clone(),
                    crate::app::usage::KEY_TEST,
                    "Reply with the single word OK.",
                    "OK?".into(),
                    CONNECTION_TEST_TOKENS,
                )
                .await
                .map(|_| model)
                .map_err(|e| connection_failure(&e, &cfg))
            },
            move |result| {
                Message::Settings(SettingsMsg::ConnectionTested {
                    number,
                    saved,
                    result,
                })
            },
        )
    }

    /// The fields changed: what a test found is not about them any more,
    /// and a test on its way is not taken.
    fn forget_connection_test(&mut self) {
        self.settings.connection_test = crate::app::state::ConnectionTest::Untried;
        self.settings.connection_tests += 1;
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
        self.settings.ai_opened = Some(crate::app::state::AiFields::of(&self.settings));
        self.settings.save_failed = false;
        // What a test found last time was about fields since filled anew.
        self.forget_connection_test();
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
                    // Not in parentheses: a provider's name may have its own
                    // ("Custom (OpenAI-compatible)").
                    tr!(
                        "Settings saved — using {provider}",
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
        self.settings.key_asked = false;
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
    /// What the provider — or, `embeddings`, the embeddings source — just
    /// picked still needs before it works: the page that asks for it, and
    /// what to say; `None` when it is ready to be kept.
    fn picked_needs(&self, embeddings: bool) -> Option<(SettingsPage, String)> {
        let s = &self.settings;
        if embeddings {
            if s.embed_here && !clew_core::embed::local::installed() {
                return Some((
                    SettingsPage::Embeddings,
                    tr!("Download the embedding model to make embeddings on this computer").into(),
                ));
            }
            if !s.embed_here && s.embed_key.is_empty() && !s.embed_key_from_env {
                return Some((
                    SettingsPage::Embeddings,
                    tr!("Add the embedding service's API key to use it").into(),
                ));
            }
            return None;
        }
        let chat = llm::Config::from_form(
            s.provider,
            s.key.clone(),
            s.model.clone(),
            s.base_url.clone(),
        );
        if chat.api_key.is_empty() {
            return Some((
                SettingsPage::LanguageModel,
                tr!(
                    "Add your {provider} API key to use it",
                    provider = s.provider.label()
                ),
            ));
        }
        None
    }

    /// Handle a [`SettingsMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_settings(&mut self, message: SettingsMsg) -> Task<Message> {
        match message {
            // Picked with Settings shut — said, or run from the command bar:
            // picked as its page would pick it, and kept as closing it
            // keeps it. Only its draft changed, which nothing then saved.
            // What the pick still needs — a key for that provider, the model
            // to make embeddings here — is asked for on its page, left open;
            // nothing is saved meanwhile. Kept, the keyboard is where it was.
            msg @ (SettingsMsg::ProviderPicked(_) | SettingsMsg::EmbedHere(_))
                if !self.settings.open =>
            {
                let focused = self.code_focused;
                let opened = self.on_open_settings();
                let embeddings = matches!(msg, SettingsMsg::EmbedHere(_));
                let picked = self.update_settings(msg);
                if let Some((page, wanted)) = self.picked_needs(embeddings) {
                    self.settings.page = page;
                    self.status = wanted;
                    return Task::batch([opened, picked]);
                }
                let closed = self.close_settings();
                if !self.settings.open {
                    self.code_focused = focused;
                }
                Task::batch([opened, picked, closed])
            }
            SettingsMsg::SetThemePref(pref) => {
                self.set_theme(pref);
                Task::none()
            }
            SettingsMsg::SetLanguage(pref) => {
                let saved = crate::i18n::apply_and_save(pref);
                self.status_follows_language();
                if let Err(e) = saved {
                    self.status = tr!("Could not save the language: {error}", error = e);
                }
                // Every window speaks it: the others are told to draw again
                // (as for an appearance change; the shell also builds the
                // macOS menu bar again), and the server that makes this
                // window's AI calls is told which language to answer in.
                theme::redraw_everywhere();
                self.send_ai_config();
                // What is said of embeddings was put in words once, in the
                // language then: said again in this one.
                self.refresh_embeddings();
                Task::batch([
                    self.respeak_explanation(),
                    self.respeak_walk_step(),
                    self.respeak_turn_endings(),
                ])
            }
            SettingsMsg::SetThemeVariant { id, is_light } => {
                self.set_theme_variant(id, is_light);
                Task::none()
            }
            SettingsMsg::ThemeResynced => {
                self.theme_pref = theme::current_pref();
                self.restyle_svgs();
                self.status_follows_language();
                // It may have been the language another window chose: this
                // window's server answers in it too.
                self.send_ai_config();
                self.refresh_embeddings();
                Task::batch([
                    self.respeak_explanation(),
                    self.respeak_walk_step(),
                    self.respeak_turn_endings(),
                ])
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
                let usage = self.on_usage_page();
                Task::batch([opened, self.sync_server_listing(), usage])
            }
            SettingsMsg::OpenAt(page) => {
                self.settings.page = page;
                let opened = self.on_open_settings();
                let usage = self.on_usage_page();
                // "Add an API Key…": the keyboard in the field it is added
                // to, while there is none.
                let key =
                    if page == SettingsPage::LanguageModel && self.settings.key.trim().is_empty() {
                        self.code_focused = false;
                        operation::focus(ui::settings_key_id())
                    } else {
                        Task::none()
                    };
                Task::batch([
                    opened,
                    self.sync_server_listing(),
                    settings_to_top(),
                    key,
                    usage,
                ])
            }
            SettingsMsg::Fix(field) => {
                use crate::app::state::LlmField;
                self.settings.page = SettingsPage::LanguageModel;
                let opened = self.on_open_settings();
                self.code_focused = false;
                let id = match field {
                    LlmField::Key => ui::settings_key_id(),
                    LlmField::Model => ui::settings_model_id(),
                    LlmField::BaseUrl => ui::settings_base_url_id(),
                };
                Task::batch([
                    opened,
                    self.sync_server_listing(),
                    settings_to_top(),
                    operation::focus(id),
                ])
            }
            SettingsMsg::NeedKey => {
                self.settings.page = SettingsPage::LanguageModel;
                let opened = self.on_open_settings();
                self.settings.key_asked = true;
                // The keyboard where the key goes.
                self.code_focused = false;
                Task::batch([
                    opened,
                    self.sync_server_listing(),
                    settings_to_top(),
                    operation::focus(ui::settings_key_id()),
                ])
            }
            SettingsMsg::Page(page) => {
                let other = self.settings.page != page;
                self.settings.page = page;
                // A shortcut half rebound stays with the page it was on.
                self.rebinding = None;
                self.keymap_notice = None;
                // Another page opens at its top: the scroll is one for
                // every page, and kept where the last page was left — the
                // Integrations page opened half way down, mid-form.
                let top = if other {
                    settings_to_top()
                } else {
                    Task::none()
                };
                let usage = if other {
                    self.on_usage_page()
                } else {
                    Task::none()
                };
                Task::batch([self.sync_server_listing(), top, usage])
            }
            SettingsMsg::Close => self.close_settings(),
            SettingsMsg::Discard => {
                // The AI fields as Settings opened on them: closing then
                // keeps nothing of what was typed.
                if let Some(opened) = self.settings.ai_opened.clone() {
                    opened.put_back(&mut self.settings);
                    self.forget_connection_test();
                    self.settings.save_failed = false;
                }
                Task::none()
            }
            SettingsMsg::ProviderPicked(p) => {
                // The drop-down sends the provider already picked too: that
                // changes nothing. It used to put the provider's defaults over
                // a model and base URL of the user's own, which closing
                // Settings then saved.
                if p == self.settings.provider {
                    return Task::none();
                }
                self.settings.provider = p;
                self.forget_connection_test();
                // Back to the provider Settings opened on: its model and base
                // URL as they were then, so a wrong pick undone costs nothing.
                // Another provider: its defaults (the user can still edit
                // them).
                // The key, too: one kept for another provider is that
                // provider's secret, never sent to this one — its own
                // variable, if it has one set, stands in.
                let (model, base_url, key) = match &self.settings.ai_snapshot.0 {
                    opened if opened.provider == p => (
                        opened.model.clone(),
                        opened.base_url.clone(),
                        opened.api_key.clone(),
                    ),
                    _ => (
                        p.default_model().to_string(),
                        p.default_base_url().to_string(),
                        String::new(),
                    ),
                };
                self.settings.key_from_env = key.is_empty()
                    && !llm::Config::from_form(p, String::new(), model.clone(), base_url.clone())
                        .api_key
                        .is_empty();
                self.settings.model = model;
                self.settings.base_url = base_url;
                self.settings.key = key;
                Task::none()
            }
            SettingsMsg::KeyChanged(s) => {
                self.settings.key = s;
                self.forget_connection_test();
                Task::none()
            }
            SettingsMsg::ModelChanged(s) => {
                self.settings.model = s;
                self.forget_connection_test();
                Task::none()
            }
            SettingsMsg::BaseUrlChanged(s) => {
                self.settings.base_url = s;
                self.forget_connection_test();
                Task::none()
            }
            SettingsMsg::TestConnection => self.on_test_connection(),
            SettingsMsg::TestSavedConnection => self.on_test_saved_connection(),
            SettingsMsg::ConnectionTested {
                number,
                saved,
                result,
            } => {
                use crate::app::state::ConnectionTest;
                let found = if saved {
                    &mut self.model_check.result
                } else {
                    &mut self.settings.connection_test
                };
                if *found == ConnectionTest::Testing(number) {
                    *found = match result {
                        Ok(model) => ConnectionTest::Answered(model),
                        Err(e) => ConnectionTest::Failed(e),
                    };
                }
                Task::none()
            }
            SettingsMsg::Saved => self.on_settings_saved(),
        }
    }
}

/// What Test connection says of a call that failed: what went wrong, what
/// to check, and the provider's or the connection's own words — what tells
/// a certificate a proxy swapped from an address that refuses
/// connections, or an account out of credit from a provider that is busy.
/// It said only the connection's words ("request failed: Connection
/// refused (os error 111)"), naming neither the address nor the field
/// that holds it.
fn connection_failure(
    err: &crate::app::rpc::CallError,
    cfg: &llm::Config,
) -> crate::app::state::TestFailure {
    crate::app::state::TestFailure::Call {
        failure: crate::app::tasks::ExplainFailure::of_call(err, &cfg.model),
        host: host_of(&cfg.base_url).to_string(),
    }
}

/// [`connection_failure`]'s failure in words, in the language spoken now.
pub(crate) fn connection_failure_said(
    failure: &crate::app::tasks::ExplainFailure,
    host: &str,
) -> String {
    use crate::app::tasks::{ExplainFailure, Rejection};
    match failure.clone() {
        ExplainFailure::Unreached(detail) => tr!(
            "Couldn't reach {host} ({detail}). Check the base URL and your network.",
            host = host,
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        // Reached, then lost or timed out while it answered: the address
        // is right, so it is not the base URL to check.
        ExplainFailure::Network(detail) => tr!(
            "The connection to {host} was lost before it answered ({detail}). Try again in a moment.",
            host = host,
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        ExplainFailure::Rejected {
            what: Rejection::Key,
            detail,
        } => tr!(
            "The provider rejected the API key ({detail}). Check it, or paste a new one.",
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        ExplainFailure::Rejected {
            what: Rejection::Model(model),
            detail,
        } => tr!(
            "The provider has no model “{model}” at this address ({detail}). Check the model's name.",
            model = model,
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        ExplainFailure::Rejected {
            what: Rejection::Endpoint,
            detail,
        } => tr!(
            "Nothing answers at this base URL ({detail}). Check it.",
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        // A 429 for the account, not the moment: out of credit, over a
        // quota. "Try again in a minute" was never true of it.
        ExplainFailure::Unavailable(detail) | ExplainFailure::Busy(detail)
            if is_about_billing(&detail) =>
        {
            tr!(
                "The provider refused for your account's quota or credit ({detail}). Check your plan.",
                detail = crate::app::tasks::plain_detail(&detail)
            )
        }
        ExplainFailure::Unavailable(detail) | ExplainFailure::Busy(detail) => tr!(
            "The provider is busy or down right now ({detail}). Try again in a minute.",
            detail = crate::app::tasks::plain_detail(&detail)
        ),
        other => {
            let said = other.describe();
            let mut chars = said.chars();
            match chars.next() {
                Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
                None => said,
            }
        }
    }
}

/// Whether a provider's words are about the account's money rather than
/// its load: OpenAI's `insufficient_quota`, a billing limit, no credit. A
/// quota per minute, a rate, a lack of capacity is load, and passes: Gemini's
/// "Quota exceeded for quota metric '… per minute'" read as out of credit.
fn is_about_billing(words: &str) -> bool {
    let words = words.to_ascii_lowercase();
    let load = [
        "per minute",
        "per second",
        "per day",
        "rate limit",
        "rate_limit",
        "capacity",
    ]
    .iter()
    .any(|w| words.contains(w));
    !load
        && ["quota", "billing", "credit", "insufficient", "payment"]
            .iter()
            .any(|w| words.contains(w))
}

/// The host and port a base URL names: what a connection that could not
/// be made was made to — never a user name or password written into it.
fn host_of(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() { base_url } else { host }
}

/// The Settings page's scroll back at its top.
fn settings_to_top() -> Task<Message> {
    iced::widget::operation::scroll_to(
        crate::ui::settings_scroll_id(),
        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test connection says what failed, what to check, and the provider's
    /// or the connection's own words: an account out of credit is not "busy",
    /// and a certificate a proxy swapped is not a wrong address. The address
    /// it names never carries a password written into the base URL.
    #[test]
    fn a_failed_connection_test_says_why_in_plain_words_and_its_own() {
        use crate::app::rpc::CallError;
        let cfg = |base: &str| {
            llm::Config::from_form(
                llm::Provider::Custom,
                String::new(),
                "m".into(),
                base.into(),
            )
        };
        let local = cfg("http://127.0.0.1:9/v1");
        let refused = CallError::Llm(llm::LlmError::Connect(
            "Connection refused (os error 111)".into(),
        ));
        let said = connection_failure(&refused, &local).said();
        assert!(said.contains("Couldn't reach 127.0.0.1:9"), "{said}");
        assert!(said.contains("nothing answered at that address"), "{said}");
        assert!(!said.contains("os error"), "{said}");
        // Reached, then lost: not the base URL to check.
        let lost = CallError::Llm(llm::LlmError::Transport(
            "timed out reading response".into(),
        ));
        let said = connection_failure(&lost, &local).said();
        assert!(said.contains("was lost before it answered"), "{said}");
        assert!(!said.contains("base URL"), "{said}");
        let tls = CallError::Llm(llm::LlmError::Connect(
            "invalid peer certificate: UnknownIssuer".into(),
        ));
        assert!(
            connection_failure(&tls, &local)
                .said()
                .contains("UnknownIssuer")
        );
        let quota = CallError::Llm(llm::LlmError::Status {
            who: "OpenAI".into(),
            code: 429,
            kind: Some("insufficient_quota".into()),
            message: "You exceeded your current quota".into(),
        });
        let said = connection_failure(&quota, &local).said();
        assert!(said.contains("quota or credit"), "{said}");
        assert!(!said.contains("busy"), "{said}");
        // Kept as what failed: said in the language spoken when it is
        // shown, not the one spoken when it was tested — the tour's sheet
        // kept its English sentence among French words.
        let failed = connection_failure(&refused, &local);
        let in_french = crate::i18n::with_lang(crate::i18n::Lang::Fr, || failed.said());
        assert!(!in_french.contains("Couldn't reach"), "{in_french}");
        assert!(in_french.contains("127.0.0.1:9"), "{in_french}");
        for (base, host) in [
            ("https://user:secret@llm.corp:8443/v1", "llm.corp:8443"),
            ("http://[::1]:8080/v1", "[::1]:8080"),
            ("llm.corp/v1", "llm.corp"),
            ("", ""),
        ] {
            assert_eq!(host_of(base), host, "{base}");
        }
    }

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
