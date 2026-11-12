//! Settings, connect, remote-browser and shortcuts modals; explain-node helpers.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use crate::SettingsPage;
use iced::widget::{column, row};

pub(crate) fn explain_child_label(node: &crate::explain::Node) -> String {
    use crate::explain::Node;
    let name = |p: &std::path::Path| {
        p.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string()
    };
    match node {
        Node::Folder(p) => format!("📁 {}", name(p)),
        Node::File(p) => name(p),
        Node::Function { name, .. } => format!("fn {name}"),
    }
}

pub(crate) fn explain_is_child(parent: &crate::explain::Node, node: &crate::explain::Node) -> bool {
    use crate::explain::Node;
    match (parent, node) {
        (Node::Folder(p), Node::Folder(c) | Node::File(c)) => c.parent() == Some(p.as_path()),
        (Node::File(p), Node::Function { file, .. }) => file == p,
        _ => false,
    }
}

/// The LLM settings modal: pick a provider, enter the API key, and optionally
/// override the model / base URL. Saved to the global `config.toml`.
///
/// The header (title, Save, Close) stays put while the form below it scrolls,
/// so the whole form is reachable in a short window. Enter in any field saves,
/// like the Save button.
use super::form;

pub(crate) fn settings_modal(app: &App) -> Element<'_, Message> {
    use crate::llm::Provider;
    // Every text field submits the form on Enter.
    let input = |placeholder: &str, value: &str, on_input: fn(String) -> Message| {
        form::input(placeholder, value)
            .on_input(on_input)
            .on_submit(Message::Settings(SettingsMsg::Saved))
    };

    let provider = form::pick(
        Provider::ALL.map(ProviderChoice),
        Some(ProviderChoice(app.settings.provider)),
        |v: ProviderChoice| Message::Settings(SettingsMsg::ProviderPicked(v.0)),
    );

    // A key that came from the environment is NOT pre-filled (see
    // `SettingsDraft::key_from_env`), so the placeholder has to explain the
    // blank field — otherwise it reads as "clew lost my key" and the user
    // pastes it in, which is what stores it.
    let key_hint = if app.settings.key_from_env {
        tr!(
            "Using {variable} from your environment",
            variable = app.settings.provider.env_key()
        )
    } else {
        tr!("Paste your API key").to_string()
    };
    let key = input(&key_hint, &app.settings.key, |v| {
        Message::Settings(SettingsMsg::KeyChanged(v))
    })
    .id(super::settings_key_id())
    .secure(true);
    let (model_hint, base_hint) = model_field_hints(app.settings.provider);
    let model = input(model_hint, &app.settings.model, |v| {
        Message::Settings(SettingsMsg::ModelChanged(v))
    })
    .id(super::settings_model_id());
    let base = input(base_hint, &app.settings.base_url, |v| {
        Message::Settings(SettingsMsg::BaseUrlChanged(v))
    })
    .id(super::settings_base_url_id());
    // What Test connection found, under its button.
    let tested = match &app.settings.connection_test {
        crate::app::state::ConnectionTest::Untried => None,
        crate::app::state::ConnectionTest::Testing(_) => Some(tr!("Testing…").to_string()),
        crate::app::state::ConnectionTest::Answered(model) => {
            Some(tr!("Connected: {model} answered.", model = model))
        }
        crate::app::state::ConnectionTest::Failed(why) => Some(why.said()),
    };
    let testing = matches!(
        app.settings.connection_test,
        crate::app::state::ConnectionTest::Testing(_)
    );
    // What it costs, said where the pointer rests: it is an AI call, as
    // the Usage page then lists it.
    let failed = matches!(
        app.settings.connection_test,
        crate::app::state::ConnectionTest::Failed(_)
    );
    let test = form::item_saying(
        tr!("Connection"),
        tested,
        failed,
        super::toolbar::tip_at(
            form::ai_action(
                tr!("Test connection"),
                (!testing).then_some(Message::Settings(SettingsMsg::TestConnection)),
            ),
            tr!("Sends one tiny AI request to check the key (a fraction of a cent)"),
            None,
            iced::widget::tooltip::Position::Top,
        ),
    );

    // Embeddings (semantic search) — an OpenAI-compatible endpoint.
    let embed_hint = if app.settings.embed_key_from_env {
        tr!(
            "Using {variable} from your environment",
            variable = "OPENAI_API_KEY"
        )
    } else {
        tr!("Embedding API key").to_string()
    };
    let embed_key = input(&embed_hint, &app.settings.embed_key, |v| {
        Message::Settings(SettingsMsg::EmbedKeyChanged(v))
    })
    .secure(true);
    let embed_model = input("text-embedding-3-small", &app.settings.embed_model, |v| {
        Message::Settings(SettingsMsg::EmbedModelChanged(v))
    });
    let embed_base = input(
        "https://api.openai.com/v1",
        &app.settings.embed_base_url,
        |v| Message::Settings(SettingsMsg::EmbedBaseUrlChanged(v)),
    );

    // System / Light / Dark, the one in use raised.
    let mode = form::segmented(crate::theme::ThemePref::ALL.map(|p| {
        (
            p.label().to_string(),
            app.theme_pref == p,
            Message::Settings(SettingsMsg::SetThemePref(p)),
        )
    }));
    // Which theme fills the light slot, and the dark.
    use crate::theme::ThemeChoice;
    let language = form::pick(
        crate::i18n::LangPref::all(),
        Some(crate::i18n::pref()),
        |p| Message::Settings(SettingsMsg::SetLanguage(p)),
    );
    let light_pick = form::pick(
        crate::theme::light_choices(),
        Some(ThemeChoice(crate::theme::current_light())),
        |c: ThemeChoice| {
            Message::Settings(SettingsMsg::SetThemeVariant {
                id: c.0.id,
                is_light: true,
            })
        },
    );
    let dark_pick = form::pick(
        crate::theme::dark_choices(),
        Some(ThemeChoice(crate::theme::current_dark())),
        |c: ThemeChoice| {
            Message::Settings(SettingsMsg::SetThemeVariant {
                id: c.0.id,
                is_light: false,
            })
        },
    );

    let page = app.settings.page;
    // The AI fields are kept as Settings closes — by ×, Esc or a click
    // outside — not as they are typed: said on their pages while any differs
    // from what Settings opened on, with the way back.
    let unsaved = || {
        app.settings_drafts_changed().then(|| {
            form::item(
                tr!("Not saved yet"),
                Some(tr!("Saved when Settings closes.").into()),
                form::action(
                    tr!("Discard changes"),
                    Some(Message::Settings(SettingsMsg::Discard)),
                ),
            )
        })
    };
    let body: Element<'_, Message> = match page {
        SettingsPage::Appearance => form::page(vec![form::group(
            "",
            None,
            None,
            vec![
                form::item(
                    tr!("Display language"),
                    // macOS draws its own parts of the window (the Open
                    // panel, alerts) in the language it was started in.
                    cfg!(target_os = "macos").then(|| {
                        tr!("The Open dialog and system alerts switch language the next time you start Clew.")
                            .to_string()
                    }),
                    language,
                ),
                form::item(tr!("Mode"), None, mode),
                form::item(tr!("Light theme"), None, light_pick),
                form::item(tr!("Dark theme"), None, dark_pick),
            ],
        )]),
        SettingsPage::Updates => {
            let mut rows = vec![
                form::toggle(
                    tr!("Check for updates automatically"),
                    None,
                    app.update.auto_check,
                    |v| Message::Updater(UpdaterMsg::SetAuto(v)),
                ),
                form::item(
                    tr!("Version"),
                    Some(format!("Clew {}", crate::updater::current_version())),
                    check_now(app),
                ),
            ];
            rows.extend(update_status(app));
            form::page(vec![form::group("", None, None, rows)])
        }
        SettingsPage::LanguageModel => form::page(vec![form::group(
            "",
            // Opened by an AI action asked without a key: why, above the
            // field it needs — it opened on the form with no word of it.
            (app.settings.key_asked && app.settings.key.trim().is_empty())
                .then(|| tr!("That needs an AI model: add an API key below, then try it again.").to_string()),
            None,
            [
                form::field(tr!("Provider"), None, provider),
                form::field(tr!("API key"), None, key),
                form::field(tr!("Model"), None, model),
                form::field(tr!("Base URL"), None, base),
                test,
            ]
            .into_iter()
            .chain(unsaved())
            .collect(),
        )]),
        SettingsPage::Embeddings => {
            let here = app.settings.embed_here;
            let source = form::segmented([
                (
                    tr!("On this computer").to_string(),
                    here,
                    Message::Settings(SettingsMsg::EmbedHere(true)),
                ),
                (
                    tr!("A service").to_string(),
                    !here,
                    Message::Settings(SettingsMsg::EmbedHere(false)),
                ),
            ]);
            let made = form::group(
                "",
                None,
                None,
                vec![form::item(tr!("Where embeddings are made"), None, source)],
            );
            let how = if here {
                form::group("", None, None, embed_model_rows(app))
            } else {
                form::group(
                    "",
                    Some(tr!("Any OpenAI-compatible /embeddings endpoint.").into()),
                    None,
                    vec![
                        form::field(tr!("API key"), None, embed_key),
                        form::field(tr!("Model"), None, embed_model),
                        form::field(tr!("Base URL"), None, embed_base),
                    ],
                )
            };
            // Last, so its coming moves none of the fields being typed in.
            let unsaved = unsaved().map(|row| form::group("", None, None, vec![row]));
            form::page([made, how].into_iter().chain(unsaved).collect())
        }
        SettingsPage::Voice => voice_section(app),
        SettingsPage::Integrations => integrations_section(app),
        SettingsPage::Languages if app.proj.project.is_none() => form::page(vec![
            form::note(tr!("Open a project to set up its languages.")).into(),
            installed_servers(app),
        ]),
        SettingsPage::Languages => languages_section(app),
        SettingsPage::Shortcuts => shortcuts_body(app),
        SettingsPage::Usage => super::usage::usage_page(app),
    };

    let mut header = row![
        text(crate::i18n::lookup(page.title()))
            .size(ts::TITLE)
            .color(theme::fg()),
        space().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if page == SettingsPage::Shortcuts && app.keymap.any_overridden() {
        header = header.push(
            button(text(tr!("Reset all")).size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(super::heard::said(
                    tr!("Reset all"),
                    Message::Window(WindowMsg::RebindResetAll),
                )),
        );
    }
    // No Save: what is changed here is kept — as it changes, or (a typed
    // field) as Settings closes.
    header = header.push(super::chrome::close_button(Message::Settings(
        SettingsMsg::Close,
    )));

    let nav_item = |p: SettingsPage| -> Element<'_, Message> {
        let on = p == page;
        button(
            row![
                glyph::icon(
                    settings_glyph(p),
                    if on {
                        theme::accent()
                    } else {
                        theme::fg_muted()
                    },
                    15.0
                ),
                text(crate::i18n::lookup(p.title()))
                    .size(ts::BASE)
                    .wrapping(Wrapping::None),
            ]
            .spacing(10)
            .align_y(iced::Center),
        )
        .style(theme::list_row(on))
        .padding([7, 10])
        .width(Fill)
        .on_press(super::heard::said(
            crate::i18n::lookup(p.title()),
            Message::Settings(SettingsMsg::Page(p)),
        ))
        .into()
    };
    let nav = container(
        iced::widget::Column::with_children(
            std::iter::once(
                container(text(tr!("Settings")).size(ts::TITLE).color(theme::fg()))
                    .padding([2, 10])
                    .into(),
            )
            .chain(std::iter::once(space().height(10).into()))
            .chain(SettingsPage::ALL.into_iter().map(nav_item)),
        )
        .spacing(2),
    )
    .width(settings_nav_w())
    .height(Fill)
    .padding([16, 10])
    .style(theme::settings_nav);

    let page_col = column![
        container(header).padding(Padding {
            top: 14.0,
            right: 14.0,
            bottom: 12.0,
            left: 24.0,
        }),
        hairline(),
        scrollable(container(body).width(Fill).padding(Padding {
            top: 16.0,
            right: 24.0 + SCROLLBAR_W,
            bottom: 20.0,
            left: 24.0,
        }))
        .id(crate::ui::settings_scroll_id())
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill),
    ]
    .width(Fill)
    .height(Fill);

    // Its size fixed — as large as it may be, within the window's margins —
    // so the modal centres it, and a page's length never moves its frame.
    let width = SETTINGS_W
        .min(app.window_width - 2.0 * MODAL_MARGIN)
        .max(320.0);
    let height = SETTINGS_MAX_H
        .min(app.window_height - 2.0 * MODAL_MARGIN)
        .max(240.0);
    let panel = container(row![nav, page_col].height(Fill))
        .width(Length::Fixed(width))
        .height(Length::Fixed(height))
        .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Settings(SettingsMsg::Close))),
    )
}

/// A provider as its picker lists it: its name, or clew-core's label for
/// one that is not a brand ("Custom"), in the interface's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderChoice(crate::llm::Provider);

impl std::fmt::Display for ProviderChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::lookup(self.0.label()))
    }
}

/// The glyph beside a page in Settings' list.
fn settings_glyph(page: SettingsPage) -> Glyph {
    match page {
        SettingsPage::Appearance => Glyph::Sun,
        SettingsPage::Updates => Glyph::Refresh,
        SettingsPage::LanguageModel => Glyph::Sparkle,
        SettingsPage::Embeddings => Glyph::FindMeaning,
        SettingsPage::Voice => Glyph::Mic,
        SettingsPage::Integrations => Glyph::Plug,
        SettingsPage::Languages => Glyph::Braces,
        SettingsPage::Shortcuts => Glyph::Shortcuts,
        SettingsPage::Usage => Glyph::Stats,
    }
}

/// A copy of `choices` changed by `change`, chosen.
fn choose_with(
    choices: &clew_protocol::LanguageChoices,
    change: impl FnOnce(&mut clew_protocol::LanguageChoices),
) -> Message {
    let mut next = choices.clone();
    change(&mut next);
    Message::Languages(crate::LanguagesMsg::Choose(Box::new(next)))
}

/// Rust targets offered beside the ones the project names.
const RUST_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "wasm32-unknown-unknown",
    "wasm32-wasip1",
    "aarch64-linux-android",
    "aarch64-apple-ios",
    "thumbv7em-none-eabihf",
];

/// What a pick list of targets or platforms calls "the host's own".
fn host_choice() -> &'static str {
    tr!("Host default")
}

/// A server program as the view shows it: the program, and its arguments
/// when they are the user's own.
fn program_line(choice: &clew_protocol::ServerChoice) -> String {
    match &choice.args {
        Some(args) if !args.is_empty() => {
            format!(
                "{} {}",
                choice.command,
                crate::app::languages::join_args(args)
            )
        }
        Some(_) => tr!("{program} (no arguments)", program = choice.command),
        None => choice.command.clone(),
    }
}

/// A typed field of the section: Enter uses it.
fn draft_field<'a>(
    app: &'a App,
    placeholder: &str,
    field: crate::LangField,
) -> iced::widget::TextInput<'a, Message> {
    use crate::LanguagesMsg as L;
    form::input(placeholder, app.proj.lang_drafts.field(field))
        .on_input(move |v| Message::Languages(L::Draft(field, v)))
        .on_submit(Message::Languages(L::UseDraft(field)))
}

/// A typed path of the user's own for `field`: the field, Use, and —
/// on this computer — Browse….
fn other_path<'a>(
    app: &'a App,
    placeholder: &str,
    field: crate::LangField,
) -> Element<'a, Message> {
    use crate::LanguagesMsg as L;
    let mut line = row![
        draft_field(app, placeholder, field),
        form::action(
            tr!("Use"),
            (!app.proj.lang_drafts.field(field).trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(field)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    // A version or flags are typed: there is no file to browse to.
    if !app.connection.is_remote() && crate::app::languages::browse_for(field).is_some() {
        line = line.push(form::action(
            tr!("Browse…"),
            Some(Message::Languages(L::Browse(field))),
        ));
    }
    form::field(tr!("Other"), None, line)
}

/// A choice that is on or off among several (a feature, a build tag).
fn chip<'a>(label: String, on: bool, msg: Message) -> Element<'a, Message> {
    button(text(label).size(ts::BODY))
        .style(theme::toggle(on))
        .padding(Padding {
            top: form::CHIP_PAD_Y,
            bottom: form::CHIP_PAD_Y,
            left: 9.0,
            right: 9.0,
        })
        .on_press(msg)
        .into()
}

/// What the project's `lsp.toml` sets that a choice here also sets.
fn project_sets_note(sets: &[String]) -> Option<String> {
    (!sets.is_empty()).then(|| {
        tr!(
            "Also set in .clew/lsp.toml: {settings}. Settings here take precedence.",
            settings = sets.join(", ")
        )
    })
}

/// A group's line under its title: what it sets, then what the project's
/// lsp.toml also sets.
fn about_with(about: &str, sets: Option<String>) -> Option<String> {
    Some(match sets {
        Some(sets) => format!("{about} {sets}"),
        None => about.to_string(),
    })
}

/// The open project's languages: each one's server and how it stands, and
/// what the user chooses for them — the Python interpreter, the Rust
/// features and target, the Go build tags and platform, the C/C++
/// compilation database and flags, Lean's and Dart's toolchains, a server
/// program of their own — from what was found where the project is
/// (`App::refresh_language_options`). A pick is saved at once, for this
/// project on this machine, and never written into the project.
/// What the model and endpoint fields say while empty: the provider's
/// defaults, which a blank field stands for. A provider of the user's own has
/// none to show: its fields say what goes in them instead of standing empty.
pub(crate) fn model_field_hints(provider: crate::llm::Provider) -> (&'static str, &'static str) {
    if provider == crate::llm::Provider::Custom {
        (
            tr!("The model's name, e.g. llama3.1"),
            tr!("Its endpoint, e.g. http://localhost:11434/v1"),
        )
    } else {
        (provider.default_model(), provider.default_base_url())
    }
}

/// `s` begun with a capital, as a line of its own begins.
fn sentence_start(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

pub(crate) fn languages_section(app: &App) -> Element<'_, Message> {
    if app.proj.project.is_none() {
        return space().height(0).into();
    }
    let choices = &app.proj.lang_choices;
    let rows = app.listed_languages();
    let mut servers = Vec::new();
    for language in &rows {
        let server = match choices.servers.as_ref().and_then(|s| s.get(language)) {
            // The server program the reader chose: "Your program:" read as
            // the reader's own code.
            Some(choice) => tr!("{program} (your choice)", program = program_line(choice)),
            None => app
                .effective_server(language)
                .map(|s| match s.command {
                    Some(_) => tr!("{server} (the project's command)", server = s.server_name),
                    None => format!("{} {}", s.server_name, s.version),
                })
                .unwrap_or_else(|| tr!("no server").to_string()),
        };
        let (status, action) = app.lsp_row(language);
        let named = crate::highlight::lang_name(language)
            .map(str::to_string)
            .unwrap_or_else(|| language.clone());
        let action: Element<'_, Message> = match action {
            Some((label, msg)) => {
                super::heard::about(&named, || form::action(label, Some(msg))).into()
            }
            None => space().width(0).into(),
        };
        servers.push(form::item(
            crate::highlight::lang_name(language)
                .map(str::to_string)
                .unwrap_or_else(|| language.clone()),
            // The server, and under it how it stands — a line of its own,
            // so begun as a sentence is: the status bar's "pyright: ready"
            // read here as a bare "ready".
            Some(format!("{server}\n{}", sentence_start(&status))),
            action,
        ));
    }

    let has = |l: &str| rows.iter().any(|x| x == l);
    let python = has("python") || choices.python.is_some();
    let listed = python
        || [
            "rust",
            "go",
            "c",
            "cpp",
            "lean",
            "dart",
            "zig",
            "typescript",
            "tsx",
            "javascript",
            "java",
            "php",
            "csharp",
            "ruby",
            "erlang",
            "haskell",
        ]
        .iter()
        .any(|l| has(l));
    let mut parts = Vec::new();
    let (options, servers_about) = match &app.proj.lang_options {
        Some(Ok(options)) => (Some(options), None),
        Some(Err(why)) => (
            None,
            Some(tr!(
                "Could not list the project's options: {error}",
                error = why
            )),
        ),
        None => (
            None,
            listed.then(|| tr!("Checking the project's options…").to_string()),
        ),
    };
    // Under the page's own title, "Language servers": its first group is
    // each language's, not the page's name again.
    parts.push(form::group(
        tr!("By language"),
        servers_about,
        listed.then(|| {
            form::action(
                tr!("Refresh"),
                Some(Message::Languages(crate::LanguagesMsg::Refresh)),
            )
            .into()
        }),
        servers,
    ));
    if python {
        parts.push(python_block(app, options.and_then(|o| o.python.as_ref())));
    }
    if has("rust") {
        parts.push(rust_block(app, options.and_then(|o| o.rust.as_ref())));
    }
    if has("go") {
        parts.push(go_block(app, options.and_then(|o| o.go.as_ref())));
    }
    if has("c") || has("cpp") {
        parts.push(c_block(app, options.and_then(|o| o.c.as_ref())));
    }
    for language in ["lean", "dart", "ruby", "erlang", "haskell"] {
        if has(language) {
            let found = options
                .and_then(|o| o.toolchains.get(language))
                .map(Vec::as_slice)
                .unwrap_or_default();
            parts.push(toolchain_block(app, language, found));
        }
    }
    let found = |language: &str| {
        options
            .and_then(|o| o.toolchains.get(language))
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    if has("typescript") || has("tsx") || has("javascript") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: tr!("TypeScript and JavaScript"),
                about: tr!("The TypeScript the language server runs."),
                automatic: tr!("Clew's built-in TypeScript 5"),
                chosen: app.proj.lang_choices.typescript.as_deref(),
                found: found("typescript"),
                field: crate::LangField::TypeScriptPath,
                placeholder: tr!("Path to tsserver.js or its folder"),
                project_note: Some(tr!(
                    "Runs the project's own code. Only for projects you trust."
                )),
            },
            |c, path| c.typescript = path,
        ));
    }
    if has("java") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: "Java",
                about: tr!("The JDK the language server uses."),
                automatic: tr!("Automatic (JAVA_HOME or PATH)"),
                chosen: app.proj.lang_choices.java.as_deref(),
                found: found("java"),
                field: crate::LangField::JavaHome,
                placeholder: tr!("Path to a JDK home folder"),
                project_note: None,
            },
            |c, path| c.java = path,
        ));
    }
    if has("php") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: "PHP",
                about: tr!("The PHP version the language server assumes."),
                automatic: tr!("The server's default"),
                chosen: app.proj.lang_choices.php.as_deref(),
                found: found("php"),
                field: crate::LangField::PhpVersion,
                placeholder: tr!("Another version, e.g. 8.2"),
                project_note: None,
            },
            |c, version| c.php = version,
        ));
    }
    if has("csharp") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: "C#",
                about: tr!("The solution the language server loads."),
                automatic: tr!("Automatic"),
                chosen: app.proj.lang_choices.csharp.as_deref(),
                found: found("csharp"),
                field: crate::LangField::CSharpSolution,
                placeholder: tr!("Path to a .sln or .slnx file"),
                project_note: None,
            },
            |c, path| c.csharp = path,
        ));
    }
    if has("zig") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: "Zig",
                about: tr!("The compiler zls uses."),
                automatic: tr!("The zig that zls finds on the PATH"),
                chosen: app.proj.lang_choices.zig.as_deref(),
                found: found("zig"),
                field: crate::LangField::ZigPath,
                placeholder: tr!("Path or name of zig"),
                project_note: None,
            },
            |c, path| c.zig = path,
        ));
    }
    parts.extend(servers_block(app));
    parts.push(installed_servers(app));
    parts.push(server_log(app));
    form::page(parts)
}

/// The servers downloaded so far — for every project, not this one only —
/// each with its version, size, and a way to remove it.
pub(crate) fn installed_servers(app: &App) -> Element<'_, Message> {
    let rows = app
        .installed_servers
        .iter()
        .map(|srv| {
            form::item(
                srv.name.clone(),
                Some(format!(
                    "{}, {}",
                    srv.version,
                    super::explain_view::human_size(srv.bytes)
                )),
                super::heard::about(&srv.name, || {
                    form::action(
                        tr!("Remove"),
                        Some(Message::Lsp(crate::LspMsg::Remove {
                            name: srv.name.clone(),
                            version: srv.version.clone(),
                        })),
                    )
                }),
            )
        })
        .collect::<Vec<_>>();
    form::group(
        tr!("Downloaded servers"),
        Some(if rows.is_empty() {
            tr!("No servers downloaded yet. They are shared by all projects.").to_string()
        } else {
            tr!("Shared by all projects.").to_string()
        }),
        None,
        rows,
    )
}

/// The newest lines of the open file's language server — copied from it
/// only when its log moved (`App::sync_lsp_log`), never per repaint — or
/// why there are none.
fn server_log(app: &App) -> Element<'_, Message> {
    use crate::LspSlot;
    let slot = app
        .active_viewer()
        .and_then(|v| v.lang_key)
        .and_then(|l| app.proj.link.lsp.get(l));
    let (lines, unreadable): (&[String], _) = match &app.proj.link.lsp_log {
        Some(tail) => match &tail.lines {
            Ok(lines) => (lines, None),
            // A poisoned state is not an empty log: say what happened.
            Err(poisoned) => (&[], Some(*poisoned)),
        },
        None => (&[], None),
    };
    let row: Element<'_, Message> = if lines.is_empty() {
        // A server that is not running has no log to show — say which, so an
        // empty box is never mistaken for a quiet but healthy server.
        let (why, color) = match (slot, unreadable) {
            (_, Some(poisoned)) => (poisoned.to_string(), theme::warning()),
            (Some(LspSlot::Ready(c)), None) if !c.alive() => (
                tr!("The server has stopped; restart it to see new output.").to_string(),
                theme::dim(),
            ),
            (Some(LspSlot::Ready(_)), None) => (tr!("No output.").to_string(), theme::dim()),
            _ => (
                tr!("No server is running for this file.").to_string(),
                theme::dim(),
            ),
        };
        form::plain(text(why).size(ts::BODY).color(color))
    } else {
        form::plain(
            scrollable(
                Column::with_children(lines.iter().map(|line| {
                    text(line.as_str())
                        .size(ts::SMALL)
                        .font(crate::fonts::CODE)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::None)
                        .into()
                }))
                .spacing(1),
            )
            .direction(both_scroll())
            .style(theme::overlay_scrollbar)
            .height(180),
        )
    };
    form::group(tr!("Server log"), None, None, vec![row])
}

/// The Python interpreter: the one in use and who named it, the ones
/// found, and a path of the user's own.
fn python_block<'a>(
    app: &'a App,
    envs: Option<&'a clew_protocol::PythonEnvironments>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    use clew_protocol::{PythonEnvKind, PythonSource};
    let remote = app.connection.is_remote();
    let chosen = app.proj.lang_choices.python.as_deref();
    let mut rows = Vec::new();
    if let Some(envs) = envs {
        let effect = &envs.in_effect;
        let who = match effect.source {
            PythonSource::Chosen => tr!("Your choice"),
            PythonSource::LspToml => tr!("Set in the project's .clew/lsp.toml"),
            PythonSource::ProjectVenv => tr!("Found in the project"),
            PythonSource::Activated => tr!("Active in the shell Clew was started from"),
            // Nobody named one: the server looks for its own. "None" read
            // as though Python went without an interpreter.
            PythonSource::Server => tr!("Chosen by the language server"),
        };
        let mut in_use = column![text(who).size(ts::BASE).color(theme::fg())].spacing(2);
        if let Some(path) = &effect.interpreter {
            in_use = in_use.push(form::path(path.clone()));
        }
        rows.push(form::field_text(tr!("In use"), None, in_use));
        if let Some(bad) = &effect.unusable_choice {
            rows.push(form::plain(
                text(tr!(
                    "The interpreter you chose ({path}) can't be used: it no longer exists, or it is a program inside the repository. The interpreter above is used until you pick another.",
                    path = bad
                ))
                .size(ts::SMALL)
                .color(theme::warning())
                .wrapping(Wrapping::WordOrGlyph),
            ));
        }
        rows.push(form::choice(
            tr!("Automatic"),
            None,
            chosen.is_none(),
            Message::Languages(L::ChoosePython(None)),
        ));
        for candidate in &envs.candidates {
            let kind = match candidate.kind {
                PythonEnvKind::Project => tr!("Project"),
                PythonEnvKind::Activated => tr!("Activated"),
                PythonEnvKind::Poetry => "Poetry",
                PythonEnvKind::Pipenv => "Pipenv",
                PythonEnvKind::Conda => "conda",
                PythonEnvKind::Pyenv => "pyenv",
                PythonEnvKind::Uv => "uv",
                PythonEnvKind::System => tr!("System"),
            };
            let label = match (&candidate.kind, &candidate.version) {
                (PythonEnvKind::System, Some(version)) => format!("{kind} Python {version}"),
                (PythonEnvKind::System, None) => kind.to_string(),
                (_, Some(version)) => format!("{} (Python {version}, {kind})", candidate.name),
                (_, None) => format!("{} ({kind})", candidate.name),
            };
            rows.push(form::choice(
                label,
                Some(form::path(candidate.interpreter.clone()).into()),
                chosen == Some(candidate.interpreter.as_str()),
                Message::Languages(L::ChoosePython(Some(candidate.interpreter.clone()))),
            ));
        }
        // A choice the list does not hold (typed, browsed to, or gone).
        if let Some(other) = chosen
            && !envs.candidates.iter().any(|c| c.interpreter == other)
        {
            rows.push(form::choice(
                tr!("Your choice"),
                Some(form::path(other.to_string()).into()),
                true,
                Message::Languages(L::ChoosePython(Some(other.to_string()))),
            ));
        }
    }
    rows.push(other_path(
        app,
        if remote {
            tr!("Python path on the remote host")
        } else {
            tr!("Path to a Python interpreter")
        },
        F::PythonPath,
    ));
    form::group(
        "Python",
        Some(tr!("The interpreter the language server uses.").into()),
        None,
        rows,
    )
}

/// The Cargo features and target rust-analyzer loads the project with.
pub(super) fn rust_block<'a>(
    app: &'a App,
    options: Option<&'a clew_protocol::RustOptions>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let rust = choices.rust.clone().unwrap_or_default();
    let check = |label: &'static str, on: bool, set: fn(&mut clew_protocol::RustChoices, bool)| {
        let press = choose_with(choices, |c| set(c.rust.get_or_insert_default(), !on));
        super::jump::mark(
            iced::widget::checkbox(on)
                .style(theme::check)
                .label(label)
                .on_toggle(move |on| {
                    choose_with(choices, |c| set(c.rust.get_or_insert_default(), on))
                })
                .text_size(ts::BASE)
                .size(15)
                .spacing(8),
            press,
        )
    };
    let mut features = column![check(
        tr!("All features of all packages"),
        rust.all_features,
        |r, on| r.all_features = on
    )]
    .spacing(10);
    if !rust.all_features {
        let toggle = |name: String| {
            choose_with(choices, move |c| {
                let features = &mut c.rust.get_or_insert_default().features;
                match features.iter().position(|f| *f == name) {
                    Some(i) => {
                        features.remove(i);
                    }
                    None => features.push(name),
                }
            })
        };
        let mut listed = Vec::new();
        for package in options.map(|o| o.packages.as_slice()).unwrap_or_default() {
            if package.features.is_empty() {
                continue;
            }
            let mut chips: Vec<Element<'a, Message>> = vec![
                text(format!("{}:", package.name))
                    .size(ts::BODY)
                    .color(theme::dim())
                    .into(),
            ];
            for feature in &package.features {
                let name = match package.root {
                    true => feature.clone(),
                    false => format!("{}/{feature}", package.name),
                };
                let label = match package.default.contains(feature) {
                    true => tr!("{feature} (default)", feature = feature),
                    false => feature.clone(),
                };
                chips.push(chip(
                    label,
                    rust.features.contains(&name),
                    toggle(name.clone()),
                ));
                listed.push(name);
            }
            features = features.push(
                Row::with_children(chips)
                    .spacing(5)
                    .align_y(iced::Center)
                    .wrap(),
            );
        }
        // Chosen ones no package lists (any more): shown, to be turned off.
        let others: Vec<Element<'a, Message>> = rust
            .features
            .iter()
            .filter(|f| !listed.contains(f))
            .map(|f| chip(f.clone(), true, toggle(f.clone())))
            .collect();
        if !others.is_empty() {
            features = features.push(Row::with_children(others).spacing(5).wrap());
        }
        features = features.push(check(
            tr!("Without default features"),
            rust.no_default_features,
            |r, on| r.no_default_features = on,
        ));
    }
    let mut targets = vec![host_choice().to_string()];
    targets.extend(options.into_iter().flat_map(|o| o.targets.iter().cloned()));
    targets.extend(RUST_TARGETS.iter().map(|t| t.to_string()));
    targets.extend(rust.target.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert(t.clone()));
    let selected = rust.target.clone().unwrap_or_else(|| host_choice().into());
    let target = column![
        form::pick(targets, Some(selected), move |t: String| {
            choose_with(choices, |c| {
                c.rust.get_or_insert_default().target = (t != host_choice()).then_some(t)
            })
        }),
        row![
            draft_field(app, tr!("Another target triple"), F::RustTarget),
            form::action(
                tr!("Use"),
                (!app.proj.lang_drafts.rust_target.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::RustTarget)))
            ),
        ]
        .spacing(6)
        .align_y(iced::Center),
    ]
    .spacing(8);
    form::group(
        "Rust",
        about_with(
            tr!("What rust-analyzer checks the project with."),
            options.and_then(|o| project_sets_note(&o.project_sets)),
        ),
        None,
        vec![
            // Its first line is a checkbox's words: on the label's line,
            // not 7 above it.
            form::field_text(tr!("Features"), None, features),
            form::field(tr!("Target"), None, target),
        ],
    )
}

/// The build tags and platform gopls loads the project with.
pub(super) fn go_block<'a>(
    app: &'a App,
    options: Option<&'a clew_protocol::GoOptions>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let go = choices.go.clone().unwrap_or_default();
    let mut all: Vec<String> = options.map(|o| o.tags.clone()).unwrap_or_default();
    all.extend(go.tags.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    all.retain(|t| seen.insert(t.clone()));
    let mut tags = column![].spacing(8);
    let no_tags = all.is_empty();
    if all.is_empty() {
        tags = tags.push(form::note(tr!("No build tags in this project.")));
    } else {
        let chips: Vec<Element<'a, Message>> = all
            .into_iter()
            .map(|tag| {
                let on = go.tags.contains(&tag);
                let msg = choose_with(choices, |c| {
                    let tags = &mut c.go.get_or_insert_default().tags;
                    match tags.iter().position(|t| *t == tag) {
                        Some(i) => {
                            tags.remove(i);
                        }
                        None => tags.push(tag.clone()),
                    }
                });
                chip(tag, on, msg)
            })
            .collect();
        tags = tags.push(Row::with_children(chips).spacing(5).wrap());
    }
    tags = tags.push(
        row![
            draft_field(app, tr!("Another build tag"), F::GoTag),
            form::action(
                tr!("Add"),
                (!app.proj.lang_drafts.go_tag.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::GoTag)))
            ),
        ]
        .spacing(6)
        .align_y(iced::Center),
    );
    let platform_pick =
        |label: &'a str,
         known: &[&str],
         value: &Option<String>,
         set: fn(&mut clew_protocol::GoChoices, Option<String>)| {
            let mut list = vec![host_choice().to_string()];
            list.extend(known.iter().map(|k| k.to_string()));
            list.extend(value.iter().cloned());
            let mut seen = std::collections::HashSet::new();
            list.retain(|t| seen.insert(t.clone()));
            let selected = value.clone().unwrap_or_else(|| host_choice().into());
            row![
                text(label)
                    .size(ts::BODY)
                    .font(crate::fonts::CODE)
                    .color(theme::dim())
                    .width(60),
                form::pick(list, Some(selected), move |v: String| {
                    choose_with(choices, |c| {
                        set(
                            c.go.get_or_insert_default(),
                            (v != host_choice()).then_some(v),
                        )
                    })
                }),
            ]
            .spacing(8)
            .align_y(iced::Center)
        };
    let platform = column![
        platform_pick("GOOS", clew_core::lsp::langopts::GOOS, &go.goos, |g, v| g
            .goos =
            v),
        platform_pick(
            "GOARCH",
            clew_core::lsp::langopts::GOARCH,
            &go.goarch,
            |g, v| { g.goarch = v }
        ),
    ]
    .spacing(8);
    form::group(
        "Go",
        about_with(
            tr!("The build tags and platform gopls uses for the project."),
            options.and_then(|o| project_sets_note(&o.project_sets)),
        ),
        None,
        vec![
            if no_tags {
                form::field_text(tr!("Build tags"), None, tags)
            } else {
                form::field_chips(tr!("Build tags"), None, tags)
            },
            form::field(tr!("Platform"), None, platform),
        ],
    )
}

/// The compilation database and flags clangd compiles C and C++ with.
fn c_block<'a>(app: &'a App, options: Option<&'a clew_protocol::COptions>) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let c = choices.c.clone().unwrap_or_default();
    let mut rows = vec![form::choice(
        tr!("Automatic"),
        Some(form::note(tr!("The compile_commands.json clangd finds.")).into()),
        c.compile_commands.is_none(),
        choose_with(choices, |c| {
            c.c.get_or_insert_default().compile_commands = None
        }),
    )];
    let found = options.map(|o| o.databases.as_slice()).unwrap_or_default();
    for dir in found {
        rows.push(form::choice(
            "compile_commands.json",
            Some(form::path(dir.clone()).into()),
            c.compile_commands.as_deref() == Some(dir.as_str()),
            choose_with(choices, |c| {
                c.c.get_or_insert_default().compile_commands = Some(dir.clone())
            }),
        ));
    }
    if let Some(dir) = &c.compile_commands
        && !found.contains(dir)
    {
        rows.push(form::choice(
            tr!("Your choice"),
            Some(form::path(dir.clone()).into()),
            true,
            choose_with(choices, |c| {
                c.c.get_or_insert_default().compile_commands = Some(dir.clone())
            }),
        ));
    }
    rows.push(other_path(
        app,
        if remote {
            tr!("compile_commands.json folder on the host")
        } else {
            tr!("Folder containing compile_commands.json")
        },
        F::CDatabase,
    ));
    let mut flags_row = row![
        draft_field(app, "-DDEBUG -Iinclude -std=c++20", F::CFlags),
        form::action(
            tr!("Use"),
            (!app.proj.lang_drafts.c_flags.trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(F::CFlags)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !c.flags.is_empty() {
        flags_row = flags_row.push(form::action(
            tr!("Clear"),
            Some(choose_with(choices, |c| {
                c.c.get_or_insert_default().flags.clear()
            })),
        ));
    }
    let flags_about = match c.flags.is_empty() {
        true => tr!("For files not in compile_commands.json.").to_string(),
        false => tr!(
            "Current: {flags}",
            flags = crate::app::languages::join_args(&c.flags)
        ),
    };
    rows.push(form::field(tr!("Flags"), Some(flags_about), flags_row));
    form::group(
        tr!("C and C++"),
        about_with(
            tr!("The compilation database and flags clangd compiles with."),
            options.and_then(|o| project_sets_note(&o.project_sets)),
        ),
        None,
        rows,
    )
}

/// A path chosen for a language from what was found, or typed: the
/// TypeScript its server runs, the `zig` zls uses.
struct PathChoice<'a> {
    title: &'static str,
    about: &'static str,
    /// What is used when nothing is chosen.
    automatic: &'static str,
    chosen: Option<&'a str>,
    found: &'a [clew_protocol::ToolchainCandidate],
    field: crate::LangField,
    placeholder: &'static str,
    /// Said when the one chosen is the project's own.
    project_note: Option<&'static str>,
}

/// A [`PathChoice`]: "Automatic", what was found, and a typed path —
/// `set` making the choice in a copy of the choices.
fn path_choice_block<'a>(
    app: &'a App,
    choice: PathChoice<'a>,
    set: fn(&mut clew_protocol::LanguageChoices, Option<String>),
) -> Element<'a, Message> {
    let choices = &app.proj.lang_choices;
    let mut rows = Vec::new();
    let chosen_is_projects = choice
        .found
        .iter()
        .any(|c| c.project && Some(c.command.as_str()) == choice.chosen);
    if let Some(note) = choice.project_note.filter(|_| chosen_is_projects) {
        rows.push(form::plain(
            text(note).size(ts::SMALL).color(theme::warning()),
        ));
    }
    rows.push(form::choice(
        tr!("Automatic"),
        Some(form::note(choice.automatic).into()),
        choice.chosen.is_none(),
        choose_with(choices, |c| set(c, None)),
    ));
    for candidate in choice.found {
        rows.push(form::choice(
            candidate.name.clone(),
            Some(form::path(candidate.command.clone()).into()),
            choice.chosen == Some(candidate.command.as_str()),
            choose_with(choices, |c| set(c, Some(candidate.command.clone()))),
        ));
    }
    if let Some(other) = choice.chosen
        && !choice.found.iter().any(|c| c.command == other)
    {
        rows.push(form::choice(
            tr!("Your choice"),
            Some(form::path(other.to_string()).into()),
            true,
            choose_with(choices, |c| set(c, Some(other.to_string()))),
        ));
    }
    rows.push(other_path(app, choice.placeholder, choice.field));
    form::group(choice.title, Some(choice.about.into()), None, rows)
}

/// Where the toolchain `language`'s server is run from (Lean's `lake`,
/// Dart's `dart`): the ones found, and clew's own lookup on `PATH`.
fn toolchain_block<'a>(
    app: &'a App,
    language: &'static str,
    found: &'a [clew_protocol::ToolchainCandidate],
) -> Element<'a, Message> {
    let choices = &app.proj.lang_choices;
    let (title, about, program) = match language {
        "lean" => (
            "Lean",
            tr!("The toolchain the language server runs from."),
            "lake",
        ),
        "ruby" => ("Ruby", tr!("The Ruby that runs Ruby LSP."), "ruby-lsp"),
        "erlang" => (
            "Erlang",
            tr!("The Erlang Language Platform build to run."),
            "elp",
        ),
        "haskell" => (
            "Haskell",
            tr!("The Haskell language server to run."),
            "haskell-language-server-wrapper",
        ),
        _ => (
            "Dart",
            tr!("The Dart SDK the language server runs from."),
            "dart",
        ),
    };
    let chosen = choices.servers.as_ref().and_then(|s| s.get(language));
    let set = |command: Option<String>| {
        choose_with(choices, |c| {
            let servers = c.servers.get_or_insert_default();
            match command {
                Some(command) => {
                    servers.insert(
                        language.to_string(),
                        clew_protocol::ServerChoice {
                            command,
                            args: None,
                        },
                    );
                }
                None => {
                    servers.remove(language);
                }
            }
        })
    };
    let mut rows = vec![form::choice(
        tr!("Automatic"),
        Some(form::note(tr!("The {program} found on the PATH", program = program)).into()),
        chosen.is_none(),
        set(None),
    )];
    for candidate in found {
        let label = match candidate.project {
            true => tr!("{name} (set by the project)", name = candidate.name),
            false => candidate.name.clone(),
        };
        rows.push(form::choice(
            label,
            Some(form::path(candidate.command.clone()).into()),
            chosen.is_some_and(|c| c.args.is_none() && c.command == candidate.command),
            set(Some(candidate.command.clone())),
        ));
    }
    // A program the list does not hold: typed under Server programs, or
    // with arguments of the user's own.
    if let Some(choice) = chosen
        && (choice.args.is_some() || !found.iter().any(|c| c.command == choice.command))
    {
        // The choice in effect: pressed again, it stays as it is. Chosen
        // again as its bare program, it lost the user's arguments to clew's
        // defaults, and its server restarted with them.
        rows.push(form::choice(
            tr!("Your own program"),
            Some(form::path(program_line(choice)).into()),
            true,
            choose_with(choices, |_| {}),
        ));
    }
    let about = match found.is_empty() {
        true => format!(
            "{about} {}",
            tr!("No other {program} found.", program = program)
        ),
        false => about.to_string(),
    };
    form::group(title, Some(about), None, rows)
}

/// The server programs the user chose, and, apart, the form to add one —
/// for a language clew serves (another build, another server) or one it
/// does not. The form sat in the chosen list's card with no heading, its
/// Use on the Arguments row as though it applied to them.
fn servers_block(app: &App) -> Vec<Element<'_, Message>> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let drafts = &app.proj.lang_drafts;
    let mut rows = Vec::new();
    for (language, choice) in choices.servers.iter().flatten() {
        let name = crate::highlight::lang_name(language).unwrap_or(language.as_str());
        let remove = choose_with(choices, |c| {
            c.servers.get_or_insert_default().remove(language);
        });
        rows.push(form::plain(
            row![
                column![
                    text(name.to_string()).size(ts::BASE).color(theme::fg()),
                    form::path(program_line(choice)),
                ]
                .spacing(2)
                .width(Fill),
                super::heard::about(name, || form::action(
                    tr!("Edit"),
                    Some(Message::Languages(L::ServerLanguage(language.clone())))
                )),
                super::heard::about(name, || form::action(tr!("Reset"), Some(remove))),
            ]
            .spacing(6)
            .align_y(iced::Center),
        ));
    }
    let mut groups = Vec::new();
    if !rows.is_empty() {
        groups.push(form::group(tr!("Server programs"), None, None, rows));
    }
    let mut rows = Vec::new();
    let mut languages = app.proj.file_languages.clone();
    languages.extend(app.proj.project_languages.iter().cloned());
    languages.sort();
    languages.dedup();
    let ready = drafts.server_language.is_some();
    rows.push(form::field(
        tr!("programming language\u{4}Language"),
        None,
        form::pick_with(
            languages,
            drafts.server_language.clone(),
            |l: String| Message::Languages(L::ServerLanguage(l)),
            Some(tr!("Choose…").to_string()),
        ),
    ));
    let mut program = row![draft_field(
        app,
        if remote {
            tr!("Program on the host (path or name)")
        } else {
            tr!("Path or name")
        },
        F::ServerCommand
    )]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        program = program.push(form::action(
            tr!("Browse…"),
            Some(Message::Languages(L::Browse(F::ServerCommand))),
        ));
    }
    rows.push(form::field(tr!("Program"), None, program));
    rows.push(form::field(
        tr!("Arguments"),
        None,
        draft_field(app, tr!("Optional"), F::ServerArgs),
    ));
    rows.push(form::plain(
        row![
            space().width(Fill),
            form::primary(
                tr!("Add"),
                (ready && !drafts.server_command.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::ServerCommand)))
            ),
        ]
        .align_y(iced::Center),
    ));
    groups.push(form::group(
        tr!("Add a server program"),
        Some(
            tr!(
                "Your own server program for a language: a different build, or a server Clew doesn't support."
            )
            .into(),
        ),
        None,
        rows,
    ));
    groups
}

/// An MCP server's approval policy as its picker lists it: clew-core's
/// label, in the interface's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApproveChoice(clew_core::mcp::Approve);

impl std::fmt::Display for ApproveChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::lookup(self.0.label()))
    }
}

/// The settings' MCP servers and ACP agents: what is configured, how each
/// stands, and forms to add more. Every change is saved at once.
fn integrations_section(app: &App) -> Element<'_, Message> {
    use crate::IntegrationsMsg as M;
    use clew_core::mcp::hub::Status;
    let draft = &app.settings.integrations;
    let small = |label: &str, msg: Message| form::action(label, Some(msg));
    let field =
        |placeholder: &str, value: &str, on_input: fn(String) -> Message, submit: Message| {
            form::input(placeholder, value)
                .on_input(on_input)
                .on_submit(submit)
        };

    let mut servers = Vec::new();
    for server in &draft.mcp {
        // Each server's controls said with its name: "Remove: github".
        let row = super::heard::about(&server.name, || {
            let (status, color) = match draft.statuses.get(&server.name) {
                _ if !server.enabled => (tr!("Off").to_string(), theme::dim()),
                Some(Status::Connected { info, tools }) => (
                    if info.name.is_empty() {
                        trn!("Connected, 1 tool", "Connected, {n} tools", *tools)
                    } else {
                        trn!(
                            "Connected to {server}, 1 tool",
                            "Connected to {server}, {n} tools",
                            *tools,
                            server = info.name
                        )
                    },
                    theme::success(),
                ),
                Some(Status::Connecting) => (tr!("Connecting…").to_string(), theme::dim()),
                Some(Status::Failed(why)) => (crate::i18n::message(why), theme::danger()),
                Some(Status::Idle) | None => (tr!("Not connected yet").to_string(), theme::dim()),
            };
            let name = server.name.clone();
            if super::heard::gathering() {
                for a in clew_core::mcp::Approve::ALL {
                    super::heard::said(
                        &ApproveChoice(a).to_string(),
                        Message::Integrations(M::McpApprove(server.name.clone(), a)),
                    );
                }
            }
            let policy = pick_list(
                clew_core::mcp::Approve::ALL.map(ApproveChoice),
                Some(ApproveChoice(server.approve)),
                move |a: ApproveChoice| Message::Integrations(M::McpApprove(name.clone(), a.0)),
            )
            .style(theme::pick)
            .menu_style(theme::pick_menu)
            .text_size(ts::BODY)
            .padding([4, 8]);
            // An HTTP server reached without a header of the user's signs in.
            let oauth = matches!(
                &server.transport,
                clew_core::mcp::Transport::Http { headers, .. }
                    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            );
            let mut actions = row![policy, space().width(Fill)]
                .spacing(6)
                .align_y(iced::Center);
            if oauth {
                actions = actions.push(if draft.signed_in.contains(&server.name) {
                    small(
                        tr!("Sign out"),
                        Message::Integrations(M::McpSignOut(server.name.clone())),
                    )
                } else {
                    small(
                        tr!("Sign in"),
                        Message::Integrations(M::McpSignIn(server.name.clone())),
                    )
                });
            }
            actions = actions
                .push(small(
                    tr!("Test"),
                    Message::Integrations(M::McpTest(server.name.clone())),
                ))
                .push(small(
                    tr!("Remove"),
                    Message::Integrations(M::McpRemove(server.name.clone())),
                ));
            let name = server.name.clone();
            let on = server.enabled;
            form::plain(
                column![
                    row![
                        column![
                            text(server.name.clone())
                                .size(ts::BASE)
                                .font(crate::fonts::UI_MEDIUM)
                                .color(theme::fg()),
                            form::path(server.summary()),
                            text(status)
                                .size(ts::SMALL)
                                .color(color)
                                .wrapping(Wrapping::WordOrGlyph),
                        ]
                        .spacing(3)
                        .width(Fill),
                        {
                            super::heard::switch(&server.name, on, |v| {
                                Message::Integrations(M::McpEnable(server.name.clone(), v))
                            });
                            form::switch(on, Message::Integrations(M::McpEnable(name, !on)))
                        },
                    ]
                    .spacing(12)
                    .align_y(iced::alignment::Vertical::Top),
                    actions,
                ]
                .spacing(10),
            )
        });
        servers.push(row);
    }
    let registry = registry_group(draft);
    // What the server last picked from the registry reads, which the form
    // leaves to the user: each variable or header, what it is for, and
    // whether it is needed and secret.
    let mut needs: Vec<Element<'_, Message>> = Vec::new();
    if let Some(used) = &draft.registry.used {
        use clew_core::mcp::registry::Install;
        let vars = match &used.install {
            Install::Stdio { env, .. } => env.as_slice(),
            Install::Remote { headers, .. } => headers.as_slice(),
            Install::Unsupported(_) => &[],
        };
        for v in vars {
            let mut tags = Vec::new();
            if v.required {
                tags.push(tr!("required"));
            }
            if v.secret {
                tags.push(tr!("secret"));
            }
            let head = if tags.is_empty() {
                v.name.clone()
            } else {
                format!("{} ({})", v.name, tags.join(", "))
            };
            needs.push(
                column![
                    text(head)
                        .size(ts::SMALL)
                        .font(crate::fonts::CODE)
                        .color(theme::fg()),
                    text(v.description.clone())
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::WordOrGlyph),
                ]
                .spacing(1)
                .into(),
            );
        }
    }
    let mut add_rows = Vec::new();
    if !needs.is_empty() {
        add_rows.push(form::field_text(
            tr!("It reads"),
            None,
            Column::with_children(needs).spacing(6),
        ));
    }
    let add_mcp = form::group(
        tr!("Add an MCP server"),
        None,
        None,
        vec![
            form::field(
                tr!("Name"),
                None,
                field(
                    tr!("e.g. github"),
                    &draft.new_mcp_name,
                    |v| Message::Integrations(M::McpName(v)),
                    Message::Integrations(M::McpAdd),
                ),
            ),
            form::field(
                tr!("Command or URL"),
                Some(tr!("Or paste an mcpServers snippet.").into()),
                field(
                    tr!("npx -y … or https://…"),
                    &draft.new_mcp_target,
                    |v| Message::Integrations(M::McpTarget(v)),
                    Message::Integrations(M::McpAdd),
                ),
            ),
            form::field(
                tr!("Environment"),
                Some(tr!("Environment variables, or headers for a URL.").into()),
                field(
                    "KEY=value; Authorization: Bearer …",
                    &draft.new_mcp_extra,
                    |v| Message::Integrations(M::McpExtra(v)),
                    Message::Integrations(M::McpAdd),
                )
                .secure(true),
            ),
            // Add only with something to add; and why it did not, beside
            // it, as the Connect form says what it lacks.
            form::plain(
                row![
                    add_said(draft.mcp_add_error.as_deref()),
                    form::primary(
                        tr!("Add server"),
                        (!draft.new_mcp_target.trim().is_empty())
                            .then_some(Message::Integrations(M::McpAdd)),
                    ),
                ]
                .spacing(12)
                .align_y(iced::Center),
            ),
        ]
        .into_iter()
        .chain(add_rows)
        .collect(),
    );

    let mut agents = Vec::new();
    for agent in &draft.acp {
        let name = agent.name.clone();
        let flip = Message::Integrations(M::AcpShareMcp(name.clone(), !agent.share_mcp));
        let share = super::jump::mark(
            iced::widget::checkbox(agent.share_mcp)
                .style(theme::check)
                .label(tr!("Share my MCP servers"))
                .on_toggle(move |on| Message::Integrations(M::AcpShareMcp(name.clone(), on)))
                .text_size(ts::BODY)
                .size(15)
                .spacing(6),
            flip,
        );
        super::heard::about_with(
            || crate::app::integrations::acp_label(agent),
            || {
                super::heard::switch(tr!("Share my MCP servers"), agent.share_mcp, |on| {
                    Message::Integrations(M::AcpShareMcp(agent.name.clone(), on))
                })
            },
        );
        let remove = super::heard::about_with(
            || crate::app::integrations::acp_label(agent),
            || {
                small(
                    tr!("Remove"),
                    Message::Integrations(M::AcpRemove(agent.name.clone())),
                )
            },
        );
        agents.push(form::plain(
            row![
                column![
                    text(crate::app::integrations::acp_label(agent))
                        .size(ts::BASE)
                        .font(crate::fonts::UI_MEDIUM)
                        .color(theme::fg()),
                    form::path(agent.summary()),
                ]
                .spacing(3)
                .width(Fill),
                share,
                remove,
            ]
            .spacing(10)
            .align_y(iced::Center),
        ));
    }
    let presets: Vec<Element<'_, Message>> = clew_core::acp::config::PRESETS
        .iter()
        .enumerate()
        .filter(|(_, p)| !draft.acp.iter().any(|a| a.name == p.name))
        .map(|(i, p)| {
            form::action(
                &format!("+ {}", p.label),
                Some(Message::Integrations(M::AcpPreset(i))),
            )
            .into()
        })
        .collect();
    let mut add_acp = Vec::new();
    if !presets.is_empty() {
        add_acp.push(form::plain(Row::with_children(presets).spacing(6).wrap()));
    }
    add_acp.push(form::field(
        tr!("Name"),
        None,
        field(
            tr!("e.g. my-agent"),
            &draft.new_acp_name,
            |v| Message::Integrations(M::AcpName(v)),
            Message::Integrations(M::AcpAdd),
        ),
    ));
    add_acp.push(form::field(
        tr!("Command"),
        Some(tr!("Starts the agent in ACP mode.").into()),
        field(
            "my-agent --acp",
            &draft.new_acp_command,
            |v| Message::Integrations(M::AcpCommand(v)),
            Message::Integrations(M::AcpAdd),
        ),
    ));
    add_acp.push(form::plain(
        row![
            add_said(draft.acp_add_error.as_deref()),
            form::primary(
                tr!("Add agent"),
                (!draft.new_acp_name.trim().is_empty() && !draft.new_acp_command.trim().is_empty())
                    .then_some(Message::Integrations(M::AcpAdd)),
            ),
        ]
        .spacing(12)
        .align_y(iced::Center),
    ));

    let mut parts = Vec::new();
    if let Some(note) = &draft.note {
        parts.push(
            text(crate::i18n::quoted(note).into_owned())
                .size(ts::BODY)
                .color(theme::fg_muted())
                .into(),
        );
    }
    for error in &draft.errors {
        parts.push(
            text(crate::i18n::quoted(&crate::i18n::message(error)).into_owned())
                .size(ts::BODY)
                .color(theme::danger())
                .into(),
        );
    }
    parts.push(form::group(
        tr!("MCP servers"),
        servers
            .is_empty()
            .then(|| tr!("No MCP servers yet. Add one to use its tools in Ask.").to_string()),
        None,
        servers,
    ));
    parts.push(registry);
    parts.push(add_mcp);
    parts.push(form::group(
        tr!("ACP agents"),
        agents
            .is_empty()
            .then(|| tr!("No agents yet. An agent can answer in Ask instead of Clew.").to_string()),
        None,
        agents,
    ));
    parts.push(form::group(tr!("Add an ACP agent"), None, None, add_acp));
    parts.push(
        form::note(tr!(
            "More options in {path}",
            path = crate::llm::config_hint()
        ))
        .into(),
    );
    form::page(parts)
}

/// Join a browsed directory with a child name, tolerating a trailing slash (so
/// the filesystem root `/` yields `/child`, not `//child`).
pub(crate) fn remote_join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The Connect modal: pick or define an SSH host, then browse its folders for
/// the one to open. Walks `ConnectStage` — picking → connecting → browsing —
/// but always in one panel so the flow reads as a single place.
pub(crate) fn connect_modal<'a>(app: &'a App) -> Element<'a, Message> {
    use crate::ConnectStage;
    let Some(ui) = &app.connect else {
        return slot();
    };

    let title = row![
        glyph::icon(Glyph::Remote, theme::accent(), 18.0),
        text(tr!("Connect to Remote"))
            .size(ts::TITLE)
            .color(theme::fg()),
        space().width(Fill),
        super::chrome::close_button(Message::Connect(ConnectMsg::Close)),
    ]
    .spacing(8)
    .align_y(iced::Center);

    let body: Element<'_, Message> = match &ui.stage {
        ConnectStage::Picking => connect_picker(app, ui, None),
        ConnectStage::Error(msg) => connect_picker(app, ui, Some(msg)),
        ConnectStage::Connecting { label } => center(
            column![
                glyph::icon(Glyph::Remote, theme::accent(), 34.0),
                text(tr!("Connecting to {host}…", host = label))
                    .size(ts::BASE)
                    .color(theme::fg()),
                text(tr!("Preparing the host…"))
                    .size(ts::SMALL)
                    .color(theme::dim()),
                space().height(6),
                button(text(tr!("Cancel")).size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([4, 14])
                    .on_press(super::heard::answer(
                        tr!("Cancel"),
                        super::heard::Answer::No,
                        Message::Connect(ConnectMsg::Close)
                    )),
            ]
            .spacing(6)
            .align_x(iced::Center),
        )
        .height(Length::Fixed(260.0))
        .into(),
        ConnectStage::Browsing(browser) => remote_browser_view(browser),
        // Each Allow is drawn enabled once its question has been within reach
        // long enough to be read, where it is (`App::asked`).
        ConnectStage::TrustHost { target, key, .. } => {
            let question = crate::app::model::Question::TrustHost(key.fingerprint.clone());
            trust_host_prompt(target, key, app.asked(question))
        }
        ConnectStage::HostKeyChanged {
            target,
            reason,
            forget_host,
        } => {
            let question = crate::app::model::Question::ForgetHostKey(forget_host.clone());
            host_key_changed_prompt(target, reason, forget_host, app.asked(question))
        }
    };

    // Always in the same slot (a zero-size one while there is nothing to say),
    // so the form's fields keep their widget state when the grant changes.
    let keys = live_ai_keys_row(app).unwrap_or_else(slot);
    // The form's scrollbar has a gutter of its own, taken out of the panel's
    // right padding: it was drawn over the Port field, Browse and Connect
    // once an error made the form scroll. Everything else keeps its edge.
    let gutter = |e: Element<'a, Message>| -> Element<'a, Message> {
        container(e)
            .padding(Padding::default().right(PICKER_GUTTER))
            .into()
    };
    let body = match &ui.stage {
        ConnectStage::Picking | ConnectStage::Error(_) => body,
        _ => gutter(body),
    };
    let panel = container(column![gutter(title.into()), gutter(keys), body].spacing(14))
        .width(DIALOG_W)
        .max_height(DIALOG_MAX_H)
        .padding(Padding {
            right: MODAL_PAD - PICKER_GUTTER,
            ..Padding::new(MODAL_PAD)
        })
        .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Connect(ConnectMsg::Close))),
    )
}

/// The unknown-host prompt: the key the host presented, its fingerprint to
/// check against one from a trusted source, and Trust / Cancel. Trust is a
/// click only — there is no text field here, so Enter can never trust a key
/// — and it names the fingerprint it was drawn for (`ConnectMsg::TrustHost`).
pub(crate) fn trust_host_prompt<'a>(
    target: &'a crate::connect::ConnTarget,
    key: &'a crate::connect::ScannedHostKey,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'a, Message> {
    let asked = asked.into();
    let ready = asked.ready;
    let host = target.label();
    // Its names — a command to run, a file — in the reader's quotation
    // marks, not backticks.
    let para = |s: String| {
        text(crate::i18n::quoted(&s).into_owned())
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
    };
    let fingerprint = container(
        column![
            text(tr!("{kind} key", kind = key.kind))
                .size(ts::CAPTION)
                .color(theme::dim()),
            text(key.fingerprint.as_str())
                .size(ts::BASE)
                .font(crate::fonts::CODE)
                .color(theme::fg()),
        ]
        .spacing(2),
    )
    .padding([8, 10])
    .width(Fill)
    .style(theme::editor);
    // Fresh for each key shown: a press held on another prompt's button is
    // not this one's (`fresh`).
    let prompt = column![
        row![
            glyph::icon(Glyph::Remote, theme::warning(), 16.0),
            text(tr!("Unknown host key"))
                .size(ts::SUBTITLE)
                .color(theme::warning()),
        ]
        .spacing(8)
        .align_y(iced::Center),
        para(tr!(
            "First connection to {host}. Trust it only if this fingerprint matches the \
             host's key.",
            host = host
        )),
        fingerprint,
        row![
            space().width(Fill),
            button(text(tr!("Cancel")).size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([6, 14])
                .on_press(super::heard::answer(
                    tr!("Cancel"),
                    super::heard::Answer::No,
                    Message::Connect(ConnectMsg::TrustCancel)
                )),
            crate::ui::fresh::allow(
                ready,
                button(text(tr!("Trust and connect")).size(ts::BODY))
                    .style(theme::primary_button)
                    .padding([6, 14])
                    .on_press_maybe(ready.then(|| {
                        super::heard::answer(
                            tr!("Trust and connect"),
                            super::heard::Answer::Yes,
                            Message::Connect(ConnectMsg::TrustHost {
                                fingerprint: key.fingerprint.clone(),
                            }),
                        )
                    }))
            ),
        ]
        .spacing(8)
        .align_y(iced::Center),
    ]
    .spacing(10);
    // Where its answers are (`placed`); fresh for each key (`fresh`).
    crate::ui::placed::placed(
        &asked,
        crate::ui::fresh::fresh_for(&key.fingerprint, prompt),
    )
}

/// The changed-host-key refusal, when the key on record is one clew trusted
/// itself: the refusal (which names the file and line), and Forget / Cancel.
/// Forget is a click only, names the host field it was drawn for
/// (`ConnectMsg::ForgetHostKey`), and trusts nothing: the reconnect it starts meets
/// the new key as unknown and shows its fingerprint to be checked first.
pub(crate) fn host_key_changed_prompt<'a>(
    target: &'a crate::connect::ConnTarget,
    reason: &'a str,
    forget_host: &'a str,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'a, Message> {
    let asked = asked.into();
    let ready = asked.ready;
    // Its names — a command to run, a file — in the reader's quotation
    // marks, not backticks.
    let para = |s: String| {
        text(crate::i18n::quoted(&s).into_owned())
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
    };
    let prompt = column![
        row![
            glyph::icon(Glyph::Remote, theme::danger(), 16.0),
            text(tr!("Host key changed"))
                .size(ts::SUBTITLE)
                .color(theme::danger()),
        ]
        .spacing(8)
        .align_y(iced::Center),
        para(reason.to_string()),
        para(tr!(
            "Forget the old key for {host} and reconnect to {target}? Only do this if you \
             know why the key changed.",
            host = forget_host,
            target = target.label()
        )),
        row![
            space().width(Fill),
            button(text(tr!("Cancel")).size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([6, 14])
                .on_press(super::heard::answer(
                    tr!("Cancel"),
                    super::heard::Answer::No,
                    Message::Connect(ConnectMsg::TrustCancel)
                )),
            crate::ui::fresh::allow(
                ready,
                button(text(tr!("Forget the old key")).size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([6, 14])
                    .on_press_maybe(ready.then(|| {
                        super::heard::answer(
                            tr!("Forget the old key"),
                            super::heard::Answer::Yes,
                            Message::Connect(ConnectMsg::ForgetHostKey {
                                host: forget_host.to_string(),
                            }),
                        )
                    }))
            ),
        ]
        .spacing(8)
        .align_y(iced::Center),
    ]
    .spacing(10);
    crate::ui::placed::placed(&asked, crate::ui::fresh::fresh_for(&forget_host, prompt))
}

/// Whether the host this window is connected to RIGHT NOW holds the AI keys,
/// with the one control that withdraws them (`ConnectMsg::RevokeAiKeys`). Kept
/// apart from the form's checkbox, which only describes the host being
/// configured — ticking it never changes what the live host holds. `None`
/// when the connection is local (there is nothing to grant).
pub(crate) fn live_ai_keys_row(app: &App) -> Option<Element<'_, Message>> {
    let held = app.live_ai_opt_in()?;
    // No key here: nothing for a host to hold, or not. The keys configured
    // (`llm_available`: config.toml or the environment), not only the
    // Settings draft — empty until Settings is opened, and never holding
    // a key from the environment.
    let has_key = app.llm_available
        || !app.settings.key.trim().is_empty()
        || !app.settings.embed_key.trim().is_empty();
    if !held && !has_key {
        return None;
    }
    let host = app.connection.label();
    let row: Element<'_, Message> = if held {
        row![
            glyph::icon(Glyph::Remote, theme::warning(), 14.0),
            text(tr!("{host} holds your API keys.", host = host))
                .size(ts::SMALL)
                .color(theme::fg()),
            space().width(Fill),
            button(text(tr!("Revoke")).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press(super::heard::said(
                    tr!("Revoke"),
                    Message::Connect(ConnectMsg::RevokeAiKeys)
                )),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into()
    } else {
        text(tr!("{host} does not hold your API keys.", host = host))
            .size(ts::SMALL)
            .color(theme::dim())
            .into()
    };
    Some(
        container(row)
            .padding([6, 10])
            .width(Fill)
            .style(theme::modal_panel)
            .into(),
    )
}

/// The picking stage: a list of saved hosts (if any) above a new-connection form.
pub(crate) fn connect_picker<'a>(
    app: &'a App,
    ui: &'a crate::ConnectUi,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    use crate::ConnectField;
    let label = |s: &str| text(s.to_string()).size(ts::SMALL).color(theme::dim());

    // What comes before the form — the saved hosts — in a place of its own,
    // there or not: pushed in front of the form's fields, it handed their
    // state on as it came and went (a host saved from another window moved
    // the User field's cursor to Name). What is wrong is said beside
    // Connect, after them: above them, it pushed the form down as it came.
    let mut head = Column::new().spacing(12);

    // Saved hosts: click a row to connect, × to forget.
    if !app.saved_connections.is_empty() {
        head = head.push(section_header(tr!("Saved hosts")));
        let mut list = Column::new().spacing(2);
        super::heard::list();
        for conn in &app.saved_connections {
            // By identity: the list is re-merged from disk whenever any
            // window saves a host, so a row's index can name another host.
            let (user_host, port) = (conn.user_host(), conn.port);
            // Where it goes, the port with it unless it is ssh's own: two
            // hosts on one machine read the same. Said once, not under a
            // title that already says it.
            let dest = if port == 22 {
                user_host.clone()
            } else {
                format!("{user_host}:{port}")
            };
            let title = conn.label();
            let mut lines =
                column![text(title.clone()).size(ts::BASE).color(theme::fg())].spacing(1);
            if dest != title {
                lines = lines.push(text(dest).size(ts::SMALL).color(theme::dim()));
            }
            let open = button(
                row![glyph::icon(Glyph::Remote, theme::fg_muted(), 14.0), lines,]
                    .spacing(8)
                    .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 10])
            .on_press(super::heard::row(
                || title.clone(),
                Message::Connect(ConnectMsg::ToSaved {
                    user_host: user_host.clone(),
                    port,
                }),
            ));
            let remove = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
                .style(theme::toolbar_button)
                .padding([5, 8])
                .on_press(super::heard::said_to(
                    tr!("Forget"),
                    &title,
                    Message::Connect(ConnectMsg::RemoveSaved { user_host, port }),
                ));
            list = list.push(row![open, remove].spacing(4).align_y(iced::Center));
        }
        head = head.push(list);
    }
    let has_head = !app.saved_connections.is_empty();
    let head =
        container(head).padding(Padding::default().bottom(if has_head { 12.0 } else { 0.0 }));
    let mut col = Column::new().spacing(12);

    // New-connection form. Enter in any field connects, like the button.
    let field = |title: &str, input: Element<'a, Message>| column![label(title), input].spacing(3);
    // A field what is wrong is about is marked, until it is edited.
    let input = |placeholder: &str, value: &str, f: ConnectField| {
        text_input(placeholder, value)
            .style(if ui.marked.contains(&f) {
                theme::input_marked
            } else {
                theme::input
            })
            .on_input(move |s| Message::Connect(ConnectMsg::Field(f, s)))
            .on_submit(Message::Connect(ConnectMsg::Submit))
            .size(ts::BASE)
            .padding(6)
    };

    let identity = row![
        input(
            tr!("(optional) ~/.ssh/id_ed25519"),
            &ui.identity,
            ConnectField::Identity
        )
        .width(Fill),
        form::action(
            tr!("Browse…"),
            Some(Message::Connect(ConnectMsg::PickIdentity))
        ),
    ]
    .spacing(6);

    col = col.push(section_header(tr!("New connection")));
    col = col.push(field(
        tr!("Name (optional)"),
        input(tr!("e.g. prod server"), &ui.name, ConnectField::Name).into(),
    ));
    col = col.push(
        row![
            field(
                tr!("Host"),
                input(
                    tr!("192.168.1.10 or example.com"),
                    &ui.host,
                    ConnectField::Host
                )
                .into()
            )
            .width(Fill),
            field(
                tr!("Port"),
                input("22", &ui.port, ConnectField::Port).into()
            )
            .width(80),
        ]
        .spacing(8),
    );
    col = col.push(field(
        tr!("User"),
        // An example, as the Name field's is: a bare "root" read as a value
        // already filled in, and Connect then said the user was missing.
        input(tr!("e.g. root"), &ui.user, ConnectField::User).into(),
    ));
    col = col.push(field(tr!("Identity file"), identity.into()));
    // Per-host consent for AI keys. Off: keys stay on this machine and AI
    // calls run locally. On: this host's clew-server receives the keys and
    // runs Ask/Explain remotely.
    col = col.push(super::jump::mark(
        iced::widget::checkbox(ui.send_ai_keys)
            .style(theme::check)
            .label(tr!("Send my API keys to this host"))
            .on_toggle({
                super::heard::switch(tr!("Send my API keys to this host"), ui.send_ai_keys, |v| {
                    Message::Connect(ConnectMsg::ToggleAiKeys(v))
                });
                |v| Message::Connect(ConnectMsg::ToggleAiKeys(v))
            })
            .size(14)
            .text_size(ts::BODY)
            .spacing(8),
        Message::Connect(ConnectMsg::ToggleAiKeys(!ui.send_ai_keys)),
    ));
    // What the box decides, said: whether the reader's paid keys leave this
    // computer. Under its label (the box's 14 px and its 8 px gap).
    col = col.push(
        container(
            text(tr!(
                "Lets Clew on that computer call your AI provider, so Ask and Explain run there. The keys go over the SSH connection and are kept in memory only, never written to disk."
            ))
            .size(ts::SMALL)
            .color(theme::fg_muted())
            .wrapping(Wrapping::WordOrGlyph),
        )
        .padding(Padding::default().left(22.0)),
    );
    let said: Element<'a, Message> = match error {
        Some(why) => text(crate::i18n::quoted(why).into_owned())
            .size(ts::BODY)
            .color(theme::danger())
            .wrapping(Wrapping::WordOrGlyph)
            .width(Fill)
            .into(),
        None => space().width(Fill).into(),
    };
    col = col.push(
        row![
            said,
            button(text(tr!("Connect")).size(ts::BASE))
                .style(theme::primary_button)
                .padding([6, 18])
                .on_press(super::heard::said(
                    tr!("Connect"),
                    Message::Connect(ConnectMsg::Submit)
                )),
        ]
        .spacing(12)
        .align_y(iced::Center),
    );

    // While connected to a remote, offer a way back to local reading.
    if app.connection.is_remote() {
        col = col.push(row![
            space().width(Fill),
            button(text(tr!("Disconnect and read local code")).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(super::heard::said(
                    tr!("Disconnect and read local code"),
                    Message::Connect(ConnectMsg::Disconnect)
                )),
        ]);
    }

    scrollable(
        container(column![head, col.width(Fill)].width(Fill))
            .padding(Padding::default().right(PICKER_GUTTER)),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Length::Shrink)
    .into()
}

/// Room at the Connect form's right for its scrollbar, out of the modal's
/// padding (`connect_modal`): the bar and a gap before the fields.
const PICKER_GUTTER: f32 = SCROLLBAR_W + 4.0;

/// The line closing a folder listing the server cut at its cap: how many
/// entries it left out (files — folders are listed first). `None` for a
/// complete listing.
pub(crate) fn omitted_note(omitted: usize) -> Option<String> {
    match omitted {
        0 => None,
        n => Some(trn!("… and 1 more", "… and {n} more", n)),
    }
}

/// The browsing stage: a path bar with an "up" control, the directory's contents
/// (folders navigable, files dimmed for context), and "Open this folder".
pub(crate) fn remote_browser_view(browser: &crate::RemoteBrowser) -> Element<'_, Message> {
    let mut up = button(glyph::icon(Glyph::ArrowLeft, theme::fg_muted(), 14.0))
        .style(theme::toolbar_button)
        .padding([4, 10]);
    if let Some(parent) = &browser.parent {
        up = up.on_press(super::heard::said(
            tr!("Up"),
            Message::Connect(ConnectMsg::BrowseTo(parent.clone())),
        ));
    }
    let path_bar = row![
        up,
        container(
            text(browser.cwd.clone())
                .size(ts::BODY)
                .font(crate::fonts::CODE)
                .color(theme::fg())
                .wrapping(Wrapping::None)
        )
        .width(Fill)
        .clip(true),
    ]
    .spacing(8)
    .align_y(iced::Center);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if browser.entries.is_empty() {
        let msg = if browser.loading {
            tr!("Loading…")
        } else {
            tr!("Empty folder.")
        };
        rows.push(
            container(text(msg).size(ts::BODY).color(theme::dim()))
                .padding([4, 8])
                .into(),
        );
    }
    super::heard::list();
    for entry in &browser.entries {
        if entry.is_dir {
            let (glyph, color) = crate::icons::folder_icon(false);
            rows.push(
                button(
                    row![
                        tree_icon(glyph, color),
                        text(entry.name.clone())
                            .size(ts::BASE)
                            .wrapping(Wrapping::None),
                    ]
                    .spacing(4)
                    .align_y(iced::Center),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([4, 8])
                .on_press(super::heard::row(
                    || entry.name.clone(),
                    Message::Connect(ConnectMsg::BrowseTo(remote_join(&browser.cwd, &entry.name))),
                ))
                .into(),
            );
        } else {
            let (glyph, color) = crate::icons::file_icon(&entry.name);
            rows.push(
                row![
                    tree_icon(glyph, color),
                    text(entry.name.clone())
                        .size(ts::BASE)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .padding([4, 8])
                .into(),
            );
        }
    }
    // A listing cut at the server's cap says so where the list ends, instead
    // of passing for the whole directory (the status line said it once).
    if let Some(note) = omitted_note(browser.omitted) {
        rows.push(
            container(
                text(note)
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .wrapping(Wrapping::Word),
            )
            .padding([4, 8])
            .into(),
        );
    }

    let entries = scrollable(Column::with_children(rows).spacing(1).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Fixed(REMOTE_LIST_H));

    let footer = row![
        column![
            text(tr!("Open this folder as the project"))
                .size(ts::SMALL)
                .color(theme::dim()),
            text(browser.cwd.clone())
                .size(ts::BODY)
                .font(crate::fonts::CODE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
        ]
        .spacing(1)
        .width(Fill),
        button(text(tr!("Open")).size(ts::BASE))
            .style(theme::primary_button)
            .padding([6, 18])
            .on_press(super::heard::said(
                tr!("Open"),
                Message::Connect(ConnectMsg::OpenHere)
            )),
    ]
    .spacing(8)
    .align_y(iced::Center);

    column![
        path_bar,
        container(entries).style(theme::modal_panel).padding(4),
        footer,
    ]
    .spacing(10)
    .into()
}

/// The keyboard shortcuts — the ones a click rebinds, and the reading
/// motions, which are fixed — as their modal and Settings' page show them.
pub(crate) fn shortcuts_body(app: &App) -> Element<'_, Message> {
    use crate::keymap::Action;

    // A one-line hint, replaced by a warning when a rebind is rejected.
    let notice: Element<'_, Message> = match &app.keymap_notice {
        Some(msg) => text(msg.clone())
            .size(ts::BODY)
            .color(theme::warning())
            .into(),
        None => form::note(tr!("Click a shortcut, then press new keys.")).into(),
    };
    // Rebindable command rows.
    let mut cmds = Vec::new();
    for action in Action::ALL {
        let binding: Element<'_, Message> = if app.rebinding == Some(action) {
            row![
                container(
                    text(tr!("Press new keys…"))
                        .size(ts::BODY)
                        .color(theme::accent()),
                )
                .padding([3, 8]),
                button(text(tr!("Cancel")).size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([3, 8])
                    .on_press(super::heard::said(
                        tr!("Cancel"),
                        Message::Window(WindowMsg::RebindCancel),
                    )),
            ]
            .spacing(4)
            .align_y(iced::Center)
            .into()
        } else {
            // Its keys as caps, as the command bar draws them: here, where
            // shortcuts are learned, they were one pill of words ("Ctrl+L")
            // and its other keys plain text beside it.
            let pill = button(super::toolbar::row_keycap(app.keymap.chord(action).caps()))
                .style(theme::secondary_button)
                .padding([2, 4])
                .on_press(super::heard::said_to_with(
                    tr!("Change shortcut"),
                    || action.label().to_string(),
                    Message::Window(WindowMsg::RebindStart(action)),
                ));
            // The chord other editors use for it, where it reaches it too:
            // said beside the one a click rebinds.
            let pill: Element<'_, Message> = match app.keymap.alias(action) {
                Some(alias) => row![
                    pill,
                    text(tr!("or")).size(ts::SMALL).color(theme::dim()),
                    super::toolbar::row_keycap(alias.caps()),
                ]
                .spacing(6)
                .align_y(iced::Center)
                .into(),
                None => pill.into(),
            };
            if app.keymap.is_overridden(action) {
                row![
                    button(text("↺").size(ts::BASE).color(theme::dim()))
                        .style(theme::toolbar_button)
                        .padding([2, 7])
                        .on_press(super::heard::said_to_with(
                            tr!("Reset"),
                            || action.label().to_string(),
                            Message::Window(WindowMsg::RebindReset(action)),
                        )),
                    pill,
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into()
            } else {
                pill
            }
        };
        cmds.push(slim(action.label().to_string(), binding));
    }

    // Read-only reading motions (not part of the customizable keymap).
    // The keys as they are named on the keyboard: Return on a Mac's,
    // Enter on others'.
    let press_keys = if cfg!(target_os = "macos") {
        "Return   Space"
    } else {
        "Enter   Space"
    };
    let motions: [(&str, &str); 25] = [
        (tr!("Move left / down / up / right"), "h j k l   ← ↓ ↑ →"),
        (tr!("Word forward / back"), "w   b"),
        (tr!("Line start / end"), "0   $   Home   End"),
        (tr!("File start / end"), "gg   G   ⌘Home   ⌘End"),
        (tr!("Page up / down"), "PgUp   PgDn"),
        (tr!("Go to definition"), "gd   F12"),
        (tr!("Find references"), "gr   ⇧F12"),
        (tr!("Go to implementation"), "gi"),
        (tr!("Go to type definition"), "gy"),
        (tr!("Call hierarchy"), "gc"),
        (tr!("Expand macro / show goal / show type"), "ge"),
        (tr!("Toggle fold"), "za"),
        (tr!("Open all folds"), "zR"),
        (tr!("Close all folds"), "zM"),
        (tr!("Time Travel: older / newer commit"), "⌘←   ⌘→"),
        (tr!("Next / previous control or field"), "Tab   ⇧Tab"),
        (tr!("Press the control the ring is on"), press_keys),
        (tr!("Code / sidebar / file tree / Ask"), "F6   ⇧F6"),
        (
            tr!("File tree: walk it, open a folder / close it"),
            "↑ ↓ → ←   j k l h",
        ),
        (tr!("Close the top panel / clear selection"), "Esc"),
        (tr!("The menu of the code or file there"), "⇧F10"),
        (tr!("Debugger: breakpoint on this line"), "F9"),
        (tr!("Debugger: start / continue, stop"), "F5   ⇧F5"),
        (tr!("Debugger: step over"), "F10"),
        (tr!("Debugger: step into / out"), "F11   ⇧F11"),
    ];
    let vim = motions
        .into_iter()
        .map(|(label, keys)| slim(label.to_string(), motion_caps(keys)))
        .collect();

    form::page(vec![
        notice,
        form::group(tr!("Commands"), None, None, cmds),
        form::group(
            tr!("Reading motions"),
            Some(tr!("Vim-style keys. These can't be changed.").into()),
            None,
            vim,
        ),
        form::note(tr!("Saved to {path}", path = crate::llm::config_hint())).into(),
    ])
}

/// A reading motion's keys as caps, as every other shortcut's are: each
/// group (three spaces apart in `keys`) its keys side by side, the groups
/// set apart. They were a line of code-font text.
fn motion_caps<'a>(keys: &str) -> Element<'a, Message> {
    let keys = crate::keymap::local_keys(keys);
    let mut groups = Row::new().spacing(10).align_y(iced::Center);
    for group in keys.split("   ").filter(|g| !g.trim().is_empty()) {
        let mut caps = Row::new().spacing(3).align_y(iced::Center);
        for key in group.split(' ').filter(|k| !k.is_empty()) {
            caps = caps.push(super::toolbar::row_keycap(key.to_string()));
        }
        groups = groups.push(caps);
    }
    groups.into()
}

/// A shortcut's row: what it does, and its keys at the right — slimmer
/// than a setting's, as the list is long.
fn slim<'a>(label: String, keys: Element<'a, Message>) -> Element<'a, Message> {
    row![
        text(label).size(ts::BASE).color(theme::fg()).width(Fill),
        keys,
    ]
    .spacing(12)
    .align_y(iced::Center)
    .padding([5, 14])
    .into()
}

/// Width of Settings' list of pages: as designed, or wider when a page's
/// name in the reader's language needs it, so none wraps.
fn settings_nav_w() -> f32 {
    // The list's padding, a row's padding, its icon and the gap after it.
    const CHROME: f32 = 2.0 * 10.0 + 2.0 * 10.0 + 15.0 + 10.0;
    SettingsPage::ALL
        .into_iter()
        .map(|p| {
            super::sheet::text_width(crate::i18n::lookup(p.title()), ts::BASE, crate::fonts::UI)
                + CHROME
        })
        .fold(SETTINGS_NAV_W, f32::max)
        .ceil()
}

/// EmbeddingGemma's rows: what it is, and at its right whether it is here —
/// Remove — its download, or getting it; under them its terms, and why its
/// last download failed.
fn embed_model_rows(app: &App) -> Vec<Element<'_, Message>> {
    use clew_core::embed::local::MODEL;
    let state: Element<'_, Message> = if let Some(d) = app.local_embedding.download.as_ref() {
        let total = d.total.unwrap_or(MODEL.bytes()).max(1);
        super::voice::getting(
            d.done,
            total,
            Message::Settings(SettingsMsg::CancelEmbedDownload),
        )
    } else if app.embed_model_installed() {
        // Just downloaded: "Ready" where Cancel was, not yet Remove — a
        // click meant for Cancel removed what had just arrived.
        let just = app
            .local_embedding
            .downloaded_at
            .is_some_and(|at| at.elapsed() < crate::app::embedding::JUST_DOWNLOADED);
        if just {
            text(tr!("Downloaded"))
                .size(ts::SMALL)
                .color(theme::success())
                .into()
        } else {
            form::action(
                tr!("Remove"),
                Some(Message::Settings(SettingsMsg::RemoveEmbedModel)),
            )
            .into()
        }
    } else {
        form::primary(
            &tr!("Download ({size} MB)", size = MODEL.size_mb()),
            Some(Message::Settings(SettingsMsg::DownloadEmbedModel)),
        )
        .into()
    };
    let mut rows = vec![form::item(
        MODEL.name,
        Some(
            tr!("By Google DeepMind, run by Clew: nothing leaves this computer, and no key is needed.")
                .to_string(),
        ),
        state,
    )];
    // Here, but nothing to run it with.
    let unrunnable = app.embed_model_installed() && !app.local_embedding.program;
    if let Some(e) = app
        .local_embedding
        .error
        .clone()
        .or_else(|| unrunnable.then(crate::app::embedding::missing_here))
        .as_ref()
    {
        rows.push(form::plain(
            text(crate::i18n::message(e))
                .size(ts::SMALL)
                .color(theme::warn())
                .wrapping(Wrapping::WordOrGlyph),
        ));
    }
    rows.push(form::item(
        tr!("Gemma Terms of Use"),
        Some(tr!("By downloading EmbeddingGemma, you accept these terms.").to_string()),
        form::action(
            tr!("Read the terms"),
            Some(Message::Content(ContentMsg::OpenLink(
                MODEL.terms.to_string(),
            ))),
        ),
    ));
    rows
}

/// The MCP registry search in Settings: what servers added here are for
/// (Clew's own agent, not an ACP agent's), the search, and what it found —
/// each with how it runs, and Use to fill the add form with it.
fn registry_group(draft: &crate::app::integrations::IntegrationsDraft) -> Element<'_, Message> {
    use crate::IntegrationsMsg as M;
    use clew_core::mcp::registry::Install;
    let reg = &draft.registry;
    let searching = reg.searching.is_some();
    let search = row![
        form::input(tr!("Search, e.g. github, postgres, browser"), &reg.query)
            .on_input(|v| Message::Integrations(M::RegistryQuery(v)))
            .on_submit(Message::Integrations(M::RegistrySearch)),
        form::action(
            if searching {
                tr!("Searching…")
            } else {
                tr!("Search")
            },
            (!searching && !reg.query.trim().is_empty())
                .then_some(Message::Integrations(M::RegistrySearch)),
        ),
    ]
    .spacing(8)
    .align_y(iced::Center);
    super::heard::field(&reg.query, |v| Message::Integrations(M::RegistryQuery(v)));
    let mut rows: Vec<Element<'_, Message>> = vec![form::plain(search)];
    match &reg.found {
        None => {}
        Some(Err(why)) => rows.push(form::plain(
            text(tr!(
                "The registry could not be searched: {why}",
                why = why.clone()
            ))
            .size(ts::BODY)
            .color(theme::danger()),
        )),
        Some(Ok(entries)) if entries.is_empty() => {
            rows.push(form::plain(form::note(tr!("Nothing found"))))
        }
        Some(Ok(entries)) => {
            for (i, entry) in entries.iter().enumerate() {
                let how = match &entry.install {
                    Install::Stdio { kind, .. } => {
                        format!("{} {}", registry_kind(kind), entry.version)
                    }
                    Install::Remote { .. } => format!("{} {}", tr!("remote"), entry.version),
                    Install::Unsupported(why) => tr!("Clew can't run it: {why}", why = why.clone()),
                };
                let usable = !matches!(entry.install, Install::Unsupported(_));
                let use_button: Element<'_, Message> = if usable {
                    super::heard::about(&entry.name, || {
                        form::action(tr!("Use"), Some(Message::Integrations(M::RegistryUse(i))))
                    })
                    .into()
                } else {
                    space().into()
                };
                rows.push(form::plain(
                    row![
                        column![
                            text(entry.name.clone())
                                .size(ts::BASE)
                                .font(crate::fonts::UI_MEDIUM)
                                .color(theme::fg())
                                .wrapping(Wrapping::WordOrGlyph),
                            text(entry.description.clone())
                                .size(ts::SMALL)
                                .color(theme::fg())
                                .wrapping(Wrapping::WordOrGlyph),
                            text(how).size(ts::SMALL).color(theme::dim()),
                        ]
                        .spacing(2)
                        .width(Fill),
                        use_button,
                    ]
                    .spacing(10)
                    .align_y(iced::Center),
                ));
            }
            rows.push(form::plain(form::note(tr!(
                "The registry lists what publishers say about their servers; it does not check them. Read the command before you add one."
            ))));
        }
    }
    form::group(tr!("Find in the MCP registry"), None, None, rows)
}

/// How a registry package is said: its ecosystem's own name.
fn registry_kind(kind: &str) -> String {
    match kind {
        "npm" => "npm".into(),
        "pypi" => "PyPI".into(),
        "oci" => "Docker".into(),
        "nuget" => "NuGet".into(),
        other => other.to_string(),
    }
}

/// "Check now", and while a check runs its icon turning and "Checking…" in
/// its place, as wide either way: pressed, it said nothing — the answer went
/// to the status bar the Settings window covers.
fn check_now(app: &App) -> Element<'_, Message> {
    use super::steady::width_of;
    let checking = app.update.checking;
    let updating = matches!(
        app.update.phase,
        crate::UpdatePhase::Downloading | crate::UpdatePhase::Installing
    );
    let idle = tr!("Check now");
    let busy = tr!("Checking…");
    const ICON: f32 = 13.0;
    const GAP: f32 = 6.0;
    let width = width_of(idle, ts::BODY, crate::fonts::UI)
        .max(width_of(busy, ts::BODY, crate::fonts::UI) + ICON + GAP)
        + 2.0 * 12.0
        + 2.0;
    let words: Element<'_, Message> = if checking {
        row![
            super::meter::spinning(Glyph::Refresh, theme::fg_muted(), ICON),
            text(busy).size(ts::BODY).wrapping(Wrapping::None),
        ]
        .spacing(GAP)
        .align_y(iced::Center)
        .into()
    } else {
        text(idle).size(ts::BODY).wrapping(Wrapping::None).into()
    };
    button(container(words).center_x(Fill))
        .style(theme::secondary_button)
        .padding([5, 12])
        .width(width)
        .on_press_maybe(
            (!checking && !updating)
                .then(|| super::heard::said(idle, Message::Updater(UpdaterMsg::Check))),
        )
        .into()
}

/// What the last check found, under the Version row: up to date, a newer
/// Clew (with what is new in it and the way to it), an update on its way,
/// or why the check failed. Nothing while a check runs — its button says
/// so — or before any.
fn update_status(app: &App) -> Option<Element<'_, Message>> {
    use crate::{CheckOutcome, UpdatePhase};
    let u = &app.update;
    if u.checking {
        return None;
    }
    let line = |mark: Element<'static, Message>, said: String, color: iced::Color| {
        row![
            mark,
            text(said)
                .size(ts::SMALL)
                .color(color)
                .wrapping(Wrapping::WordOrGlyph)
                .width(Fill),
        ]
        .spacing(6)
        .align_y(iced::Center)
    };
    let mark = |g: Glyph, color: iced::Color| -> Element<'static, Message> {
        crate::glyph::icon(g, color, 13.0)
    };
    let body: Element<'_, Message> = match (&u.phase, &u.available, &u.last_check) {
        (UpdatePhase::Downloading, Some(a), _) => line(
            super::meter::spinning(Glyph::Refresh, theme::accent(), 13.0),
            tr!("Downloading Clew {version}…", version = a.version),
            theme::fg_muted(),
        )
        .into(),
        (UpdatePhase::Installing, Some(_), _) => line(
            super::meter::spinning(Glyph::Refresh, theme::accent(), 13.0),
            tr!("Installing update… Clew will relaunch").into(),
            theme::fg_muted(),
        )
        .into(),
        (UpdatePhase::Failed(e), Some(_), _) => line(
            mark(Glyph::Info, theme::danger()),
            tr!("Update failed: {error}", error = e),
            theme::danger(),
        )
        .push(form::action(
            tr!("Retry"),
            Some(Message::Updater(UpdaterMsg::InstallStart)),
        ))
        .into(),
        (_, Some(a), _) => line(
            mark(Glyph::Sparkle, theme::accent()),
            tr!("Clew {version} is available", version = a.version),
            theme::fg(),
        )
        .push(form::action(
            tr!("What's new"),
            Some(Message::Updater(UpdaterMsg::ShowNotes)),
        ))
        .push(form::primary(
            tr!("Update now"),
            Some(Message::Updater(UpdaterMsg::InstallStart)),
        ))
        .into(),
        (_, None, Some(CheckOutcome::UpToDate)) => line(
            mark(Glyph::CheckCircle, theme::success()),
            tr!("Clew is up to date").into(),
            theme::fg_muted(),
        )
        .into(),
        (_, None, Some(CheckOutcome::Failed(e))) => line(
            mark(Glyph::Info, theme::danger()),
            tr!("Update check failed: {error}", error = e),
            theme::danger(),
        )
        .into(),
        _ => return None,
    };
    Some(form::row_of(body))
}

/// Why an Add of the Integrations page's forms did not add, in the danger
/// colour, filling the row up to its button; nothing, and the row is the
/// same height, when it did.
fn add_said<'a>(error: Option<&str>) -> Element<'a, Message> {
    match error {
        Some(why) => {
            text(crate::i18n::quoted(&tr!("Not added: {error}.", error = why)).into_owned())
                .size(ts::BODY)
                .color(theme::danger())
                .wrapping(Wrapping::WordOrGlyph)
                .width(Fill)
                .into()
        }
        None => space().width(Fill).into(),
    }
}
