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

    let provider = form::pick(&Provider::ALL[..], Some(app.settings.provider), |v| {
        Message::Settings(SettingsMsg::ProviderPicked(v))
    });

    // A key that came from the environment is NOT pre-filled (see
    // `SettingsDraft::key_from_env`), so the placeholder has to explain the
    // blank field — otherwise it reads as "clew lost my key" and the user
    // pastes it in, which is what stores it.
    let key_hint = if app.settings.key_from_env {
        format!(
            "using {} from your environment",
            app.settings.provider.env_key()
        )
    } else {
        "paste your API key".to_string()
    };
    let key = input(&key_hint, &app.settings.key, |v| {
        Message::Settings(SettingsMsg::KeyChanged(v))
    })
    .secure(true);
    let model = input(
        app.settings.provider.default_model(),
        &app.settings.model,
        |v| Message::Settings(SettingsMsg::ModelChanged(v)),
    );
    let base = input(
        app.settings.provider.default_base_url(),
        &app.settings.base_url,
        |v| Message::Settings(SettingsMsg::BaseUrlChanged(v)),
    );

    // Embeddings (semantic search) — an OpenAI-compatible endpoint.
    let embed_hint = if app.settings.embed_key_from_env {
        "using OPENAI_API_KEY from your environment"
    } else {
        "embedding API key"
    };
    let embed_key = input(embed_hint, &app.settings.embed_key, |v| {
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
    let stored = || form::note(format!("Stored in {}", crate::llm::config_hint())).into();
    let body: Element<'_, Message> = match page {
        SettingsPage::Appearance => form::page(vec![form::group(
            "",
            None,
            None,
            vec![
                form::item(crate::tr!("Language"), None, language),
                form::item("Mode", None, mode),
                form::item("Light theme", None, light_pick),
                form::item("Dark theme", None, dark_pick),
            ],
        )]),
        SettingsPage::Updates => form::page(vec![form::group(
            "",
            None,
            None,
            vec![
                form::toggle(
                    "Check for updates automatically",
                    None,
                    app.update.auto_check,
                    |v| Message::Updater(UpdaterMsg::SetAuto(v)),
                ),
                form::item(
                    "Version",
                    Some(format!("clew {}", crate::updater::current_version())),
                    form::action("Check now", Some(Message::Updater(UpdaterMsg::Check))),
                ),
            ],
        )]),
        SettingsPage::LanguageModel => form::page(vec![
            form::group(
                "",
                None,
                None,
                vec![
                    form::field("Provider", None, provider),
                    form::field("API key", None, key),
                    form::field("Model", None, model),
                    form::field("Base URL", None, base),
                ],
            ),
            stored(),
        ]),
        SettingsPage::Embeddings => form::page(vec![
            form::group(
                "",
                Some("Any OpenAI-compatible /embeddings endpoint.".into()),
                None,
                vec![
                    form::field("API key", None, embed_key),
                    form::field("Model", None, embed_model),
                    form::field("Base URL", None, embed_base),
                ],
            ),
            stored(),
        ]),
        SettingsPage::Voice => voice_section(app),
        SettingsPage::Integrations => integrations_section(app),
        SettingsPage::Languages if app.proj.project.is_none() => form::page(vec![
            form::note("Open a project to set up its languages.").into(),
            installed_servers(app),
        ]),
        SettingsPage::Languages => languages_section(app),
        SettingsPage::Shortcuts => shortcuts_body(app),
    };

    let mut header = row![
        text(page.title()).size(ts::TITLE).color(theme::fg()),
        space().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if page == SettingsPage::Shortcuts && app.keymap.any_overridden() {
        header = header.push(
            button(text("Reset all").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::Window(WindowMsg::RebindResetAll)),
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
                text(p.title()).size(ts::BASE),
            ]
            .spacing(10)
            .align_y(iced::Center),
        )
        .style(theme::list_row(on))
        .padding([7, 10])
        .width(Fill)
        .on_press(Message::Settings(SettingsMsg::Page(p)))
        .into()
    };
    let nav = container(
        iced::widget::Column::with_children(
            std::iter::once(
                container(text("Settings").size(ts::TITLE).color(theme::fg()))
                    .padding([2, 10])
                    .into(),
            )
            .chain(std::iter::once(space().height(10).into()))
            .chain(SettingsPage::ALL.into_iter().map(nav_item)),
        )
        .spacing(2),
    )
    .width(SETTINGS_NAV_W)
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
const HOST_CHOICE: &str = "The host's";

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
        Some(_) => format!("{} (no arguments)", choice.command),
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
            "Use",
            (!app.proj.lang_drafts.field(field).trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(field)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !app.connection.is_remote() {
        line = line.push(form::action(
            "Browse…",
            Some(Message::Languages(L::Browse(field))),
        ));
    }
    form::field("Other", None, line)
}

/// A choice that is on or off among several (a feature, a build tag).
fn chip<'a>(label: String, on: bool, msg: Message) -> Element<'a, Message> {
    button(text(label).size(ts::BODY))
        .style(theme::toggle(on))
        .padding([3, 9])
        .on_press(msg)
        .into()
}

/// What the project's `lsp.toml` sets that a choice here also sets.
fn project_sets_note(sets: &[String]) -> Option<String> {
    (!sets.is_empty()).then(|| {
        format!(
            "Also set in .clew/lsp.toml: {}. Settings here win.",
            sets.join(", ")
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
pub(crate) fn languages_section(app: &App) -> Element<'_, Message> {
    if app.proj.project.is_none() {
        return space().height(0).into();
    }
    let choices = &app.proj.lang_choices;
    let mut rows = app.proj.project_languages.clone();
    if let Some(servers) = &choices.servers {
        rows.extend(servers.keys().cloned());
    }
    rows.sort();
    rows.dedup();
    let mut servers = Vec::new();
    for language in &rows {
        let server = match choices.servers.as_ref().and_then(|s| s.get(language)) {
            Some(choice) => format!("Your program: {}", program_line(choice)),
            None => app
                .effective_server(language)
                .map(|s| match s.command {
                    Some(_) => format!("{} (the project's command)", s.server_name),
                    None => format!("{} {}", s.server_name, s.version),
                })
                .unwrap_or_else(|| "no server".into()),
        };
        let (status, action) = app.lsp_row(language);
        let action: Element<'_, Message> = match action {
            Some((label, msg)) => form::action(label, Some(msg)).into(),
            None => space().width(0).into(),
        };
        servers.push(form::item(
            crate::highlight::lang_name(language)
                .map(str::to_string)
                .unwrap_or_else(|| language.clone()),
            Some(format!("{server} · {status}")),
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
            Some(format!(
                "What the project offers could not be listed: {why}"
            )),
        ),
        None => (
            None,
            listed.then(|| "Looking at what the project offers…".to_string()),
        ),
    };
    parts.push(form::group(
        "Language servers",
        servers_about,
        listed.then(|| {
            form::action(
                "Look again",
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
                title: "TypeScript and JavaScript",
                about: "The TypeScript its server runs.",
                automatic: "clew's TypeScript 5",
                chosen: app.proj.lang_choices.typescript.as_deref(),
                found: found("typescript"),
                field: crate::LangField::TypeScriptPath,
                placeholder: "Path to tsserver.js or its folder",
                project_note: Some("Runs the project's own code. Only for projects you trust."),
            },
            |c, path| c.typescript = path,
        ));
    }
    if has("java") {
        parts.push(path_choice_block(
            app,
            PathChoice {
                title: "Java",
                about: "The JDK its server reads the project with.",
                automatic: "Automatic (JAVA_HOME or PATH)",
                chosen: app.proj.lang_choices.java.as_deref(),
                found: found("java"),
                field: crate::LangField::JavaHome,
                placeholder: "A JDK's home folder",
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
                about: "The version its server reads the project as.",
                automatic: "The server's own default",
                chosen: app.proj.lang_choices.php.as_deref(),
                found: found("php"),
                field: crate::LangField::PhpVersion,
                placeholder: "Another version, e.g. 8.2",
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
                about: "The solution its server loads.",
                automatic: "Automatic",
                chosen: app.proj.lang_choices.csharp.as_deref(),
                found: found("csharp"),
                field: crate::LangField::CSharpSolution,
                placeholder: "A .sln or .slnx file",
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
                about: "The compiler zls uses.",
                automatic: "The zig zls finds on the PATH",
                chosen: app.proj.lang_choices.zig.as_deref(),
                found: found("zig"),
                field: crate::LangField::ZigPath,
                placeholder: "Path or name of zig",
                project_note: None,
            },
            |c, path| c.zig = path,
        ));
    }
    parts.push(servers_block(app));
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
                    "{} · {}",
                    srv.version,
                    super::explain_view::human_size(srv.bytes)
                )),
                form::action(
                    "Remove",
                    Some(Message::Lsp(crate::LspMsg::Remove {
                        name: srv.name.clone(),
                        version: srv.version.clone(),
                    })),
                ),
            )
        })
        .collect::<Vec<_>>();
    form::group(
        "Downloaded servers",
        Some(if rows.is_empty() {
            "None yet. They are shared by every project.".to_string()
        } else {
            "Shared by every project.".to_string()
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
                "The server has stopped; restart it to see new output.".to_string(),
                theme::dim(),
            ),
            (Some(LspSlot::Ready(_)), None) => ("No output.".to_string(), theme::dim()),
            _ => ("No server running for this file.".to_string(), theme::dim()),
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
    form::group(
        "Server log",
        Some("The open file's server, newest last.".into()),
        None,
        vec![row],
    )
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
            PythonSource::Chosen => "Your choice",
            PythonSource::LspToml => "Named by the project's .clew/lsp.toml",
            PythonSource::ProjectVenv => "Found in the project",
            PythonSource::Activated => "Active where clew started",
            PythonSource::Server => "None",
        };
        let mut in_use = column![text(who).size(ts::BASE).color(theme::fg())].spacing(2);
        if let Some(path) = &effect.interpreter {
            in_use = in_use.push(form::path(path.clone()));
        }
        rows.push(form::field("In use", None, in_use));
        if let Some(bad) = &effect.unusable_choice {
            rows.push(form::plain(
                text(format!(
                    "Your choice {bad} cannot be used: it is gone, or it is a program of \
                     the repository's own. The one above is used until you pick another."
                ))
                .size(ts::SMALL)
                .color(theme::warning())
                .wrapping(Wrapping::WordOrGlyph),
            ));
        }
        rows.push(form::choice(
            "Automatic",
            None,
            chosen.is_none(),
            Message::Languages(L::ChoosePython(None)),
        ));
        for candidate in &envs.candidates {
            let kind = match candidate.kind {
                PythonEnvKind::Project => "Project",
                PythonEnvKind::Activated => "Activated",
                PythonEnvKind::Poetry => "Poetry",
                PythonEnvKind::Pipenv => "Pipenv",
                PythonEnvKind::Conda => "conda",
                PythonEnvKind::Pyenv => "pyenv",
                PythonEnvKind::Uv => "uv",
                PythonEnvKind::System => "System",
            };
            let label = match (&candidate.kind, &candidate.version) {
                (PythonEnvKind::System, Some(version)) => format!("{kind} · Python {version}"),
                (PythonEnvKind::System, None) => kind.to_string(),
                (_, Some(version)) => format!("{kind} · {} · Python {version}", candidate.name),
                (_, None) => format!("{kind} · {}", candidate.name),
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
                "Your choice",
                Some(form::path(other.to_string()).into()),
                true,
                Message::Languages(L::ChoosePython(Some(other.to_string()))),
            ));
        }
    }
    rows.push(other_path(
        app,
        if remote {
            "Python path on the remote host"
        } else {
            "Path to a Python interpreter"
        },
        F::PythonPath,
    ));
    form::group(
        "Python",
        Some("The interpreter its server reads the project with.".into()),
        None,
        rows,
    )
}

/// The Cargo features and target rust-analyzer loads the project with.
fn rust_block<'a>(
    app: &'a App,
    options: Option<&'a clew_protocol::RustOptions>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let rust = choices.rust.clone().unwrap_or_default();
    let check = |label: &'static str, on: bool, set: fn(&mut clew_protocol::RustChoices, bool)| {
        iced::widget::checkbox(on)
            .style(theme::check)
            .label(label)
            .on_toggle(move |on| choose_with(choices, |c| set(c.rust.get_or_insert_default(), on)))
            .text_size(ts::BASE)
            .size(15)
            .spacing(8)
    };
    let mut features = column![check(
        "Every feature of every package",
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
                    true => format!("{feature} (default)"),
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
            "Without the packages' default features",
            rust.no_default_features,
            |r, on| r.no_default_features = on,
        ));
    }
    let mut targets = vec![HOST_CHOICE.to_string()];
    targets.extend(options.into_iter().flat_map(|o| o.targets.iter().cloned()));
    targets.extend(RUST_TARGETS.iter().map(|t| t.to_string()));
    targets.extend(rust.target.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert(t.clone()));
    let selected = rust.target.clone().unwrap_or_else(|| HOST_CHOICE.into());
    let target = column![
        form::pick(targets, Some(selected), move |t: String| {
            choose_with(choices, |c| {
                c.rust.get_or_insert_default().target = (t != HOST_CHOICE).then_some(t)
            })
        }),
        row![
            draft_field(app, "Another target triple", F::RustTarget),
            form::action(
                "Use",
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
            "What rust-analyzer checks the project with.",
            options.and_then(|o| project_sets_note(&o.project_sets)),
        ),
        None,
        vec![
            form::field("Features", None, features),
            form::field("Target", None, target),
        ],
    )
}

/// The build tags and platform gopls loads the project with.
fn go_block<'a>(
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
    if all.is_empty() {
        tags = tags.push(form::note("None in this project."));
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
            draft_field(app, "Another build tag", F::GoTag),
            form::action(
                "Add",
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
            let mut list = vec![HOST_CHOICE.to_string()];
            list.extend(known.iter().map(|k| k.to_string()));
            list.extend(value.iter().cloned());
            let mut seen = std::collections::HashSet::new();
            list.retain(|t| seen.insert(t.clone()));
            let selected = value.clone().unwrap_or_else(|| HOST_CHOICE.into());
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
                            (v != HOST_CHOICE).then_some(v),
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
            "The build tags and platform gopls reads the project for.",
            options.and_then(|o| project_sets_note(&o.project_sets)),
        ),
        None,
        vec![
            form::field("Build tags", None, tags),
            form::field("Platform", None, platform),
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
        "Automatic",
        Some(form::note("The compile_commands.json clangd finds.").into()),
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
            "Your choice",
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
            "compile_commands.json folder on the host"
        } else {
            "Folder holding compile_commands.json"
        },
        F::CDatabase,
    ));
    let mut flags_row = row![
        draft_field(app, "-DDEBUG -Iinclude -std=c++20", F::CFlags),
        form::action(
            "Use",
            (!app.proj.lang_drafts.c_flags.trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(F::CFlags)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !c.flags.is_empty() {
        flags_row = flags_row.push(form::action(
            "Clear",
            Some(choose_with(choices, |c| {
                c.c.get_or_insert_default().flags.clear()
            })),
        ));
    }
    let flags_about = match c.flags.is_empty() {
        true => "For files the database does not cover.".to_string(),
        false => format!("Now: {}", crate::app::languages::join_args(&c.flags)),
    };
    rows.push(form::field("Flags", Some(flags_about), flags_row));
    form::group(
        "C and C++",
        about_with(
            "The compilation database and flags clangd compiles with.",
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
        "Automatic",
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
            "Your choice",
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
        "lean" => ("Lean", "The toolchain its server runs from.", "lake"),
        "ruby" => ("Ruby", "The Ruby whose Ruby LSP runs.", "ruby-lsp"),
        "erlang" => ("Erlang", "The language platform its server is.", "elp"),
        "haskell" => (
            "Haskell",
            "The language server it runs.",
            "haskell-language-server-wrapper",
        ),
        _ => ("Dart", "The SDK its server runs from.", "dart"),
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
        "Automatic",
        Some(form::note(format!("The {program} found on the PATH")).into()),
        chosen.is_none(),
        set(None),
    )];
    for candidate in found {
        let label = match candidate.project {
            true => format!("{} — the project's", candidate.name),
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
        rows.push(form::choice(
            "Your own program",
            Some(form::path(program_line(choice)).into()),
            true,
            set(Some(choice.command.clone())),
        ));
    }
    let about = match found.is_empty() {
        true => format!("{about} No other {program} found."),
        false => about.to_string(),
    };
    form::group(title, Some(about), None, rows)
}

/// The server programs the user chose, and the form to choose one — for a
/// language clew serves (another build, another server) or one it does not.
fn servers_block(app: &App) -> Element<'_, Message> {
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
                form::action(
                    "Edit",
                    Some(Message::Languages(L::ServerLanguage(language.clone())))
                ),
                form::action("Reset", Some(remove)),
            ]
            .spacing(6)
            .align_y(iced::Center),
        ));
    }
    let mut languages = app.proj.file_languages.clone();
    languages.extend(app.proj.project_languages.iter().cloned());
    languages.sort();
    languages.dedup();
    let ready = drafts.server_language.is_some();
    rows.push(form::field(
        "Language",
        None,
        form::pick(languages, drafts.server_language.clone(), |l: String| {
            Message::Languages(L::ServerLanguage(l))
        })
        .placeholder("Choose…"),
    ));
    let mut program = row![draft_field(
        app,
        if remote {
            "Program on the host (path or name)"
        } else {
            "Path or name"
        },
        F::ServerCommand
    )]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        program = program.push(form::action(
            "Browse…",
            Some(Message::Languages(L::Browse(F::ServerCommand))),
        ));
    }
    rows.push(form::field("Program", None, program));
    rows.push(form::field(
        "Arguments",
        None,
        row![
            draft_field(app, "Optional", F::ServerArgs),
            form::action(
                "Use",
                (ready && !drafts.server_command.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::ServerCommand)))
            ),
        ]
        .spacing(6)
        .align_y(iced::Center),
    ));
    form::group(
        "Server programs",
        Some(
            "A program of your own for a language: another build, or a server clew does not know."
                .into(),
        ),
        None,
        rows,
    )
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
        let (status, color) = match draft.statuses.get(&server.name) {
            _ if !server.enabled => ("Off".to_string(), theme::dim()),
            Some(Status::Connected { info, tools }) => (
                format!(
                    "Connected{} · {tools} tools",
                    if info.name.is_empty() {
                        String::new()
                    } else {
                        format!(" to {}", info.name)
                    }
                ),
                theme::success(),
            ),
            Some(Status::Connecting) => ("Connecting…".to_string(), theme::dim()),
            Some(Status::Failed(why)) => (why.clone(), theme::danger()),
            Some(Status::Idle) | None => ("Not connected yet".to_string(), theme::dim()),
        };
        let name = server.name.clone();
        let policy = pick_list(
            &clew_core::mcp::Approve::ALL[..],
            Some(server.approve),
            move |a| Message::Integrations(M::McpApprove(name.clone(), a)),
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
                    "Sign out",
                    Message::Integrations(M::McpSignOut(server.name.clone())),
                )
            } else {
                small(
                    "Sign in",
                    Message::Integrations(M::McpSignIn(server.name.clone())),
                )
            });
        }
        actions = actions
            .push(small(
                "Test",
                Message::Integrations(M::McpTest(server.name.clone())),
            ))
            .push(small(
                "Remove",
                Message::Integrations(M::McpRemove(server.name.clone())),
            ));
        let name = server.name.clone();
        let on = server.enabled;
        servers.push(form::plain(
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
                    form::switch(on, Message::Integrations(M::McpEnable(name, !on))),
                ]
                .spacing(12)
                .align_y(iced::alignment::Vertical::Top),
                actions,
            ]
            .spacing(10),
        ));
    }
    let add_mcp = form::group(
        "Add an MCP server",
        None,
        None,
        vec![
            form::field(
                "Name",
                None,
                field(
                    "e.g. github",
                    &draft.new_mcp_name,
                    |v| Message::Integrations(M::McpName(v)),
                    Message::Integrations(M::McpAdd),
                ),
            ),
            form::field(
                "Command or URL",
                Some("Or paste an mcpServers snippet.".into()),
                field(
                    "npx -y … or https://…",
                    &draft.new_mcp_target,
                    |v| Message::Integrations(M::McpTarget(v)),
                    Message::Integrations(M::McpAdd),
                ),
            ),
            form::field(
                "Environment",
                Some("Headers, for a URL.".into()),
                field(
                    "KEY=value; Authorization: Bearer …",
                    &draft.new_mcp_extra,
                    |v| Message::Integrations(M::McpExtra(v)),
                    Message::Integrations(M::McpAdd),
                )
                .secure(true),
            ),
            form::plain(
                row![
                    space().width(Fill),
                    form::primary("Add server", Some(Message::Integrations(M::McpAdd))),
                ]
                .align_y(iced::Center),
            ),
        ],
    );

    let mut agents = Vec::new();
    for agent in &draft.acp {
        let name = agent.name.clone();
        let share = iced::widget::checkbox(agent.share_mcp)
            .style(theme::check)
            .label("Share my MCP servers")
            .on_toggle(move |on| Message::Integrations(M::AcpShareMcp(name.clone(), on)))
            .text_size(ts::BODY)
            .size(15)
            .spacing(6);
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
                small(
                    "Remove",
                    Message::Integrations(M::AcpRemove(agent.name.clone()))
                ),
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
        "Name",
        None,
        field(
            "e.g. my-agent",
            &draft.new_acp_name,
            |v| Message::Integrations(M::AcpName(v)),
            Message::Integrations(M::AcpAdd),
        ),
    ));
    add_acp.push(form::field(
        "Command",
        Some("Starts it in ACP mode.".into()),
        field(
            "my-agent --acp",
            &draft.new_acp_command,
            |v| Message::Integrations(M::AcpCommand(v)),
            Message::Integrations(M::AcpAdd),
        ),
    ));
    add_acp.push(form::plain(
        row![
            space().width(Fill),
            form::primary("Add agent", Some(Message::Integrations(M::AcpAdd))),
        ]
        .align_y(iced::Center),
    ));

    let mut parts = Vec::new();
    if let Some(note) = &draft.note {
        parts.push(
            text(note.clone())
                .size(ts::BODY)
                .color(theme::fg_muted())
                .into(),
        );
    }
    for error in &draft.errors {
        parts.push(
            text(error.clone())
                .size(ts::BODY)
                .color(theme::danger())
                .into(),
        );
    }
    parts.push(form::group(
        "MCP servers",
        servers
            .is_empty()
            .then(|| "None yet. Their tools are offered to Ask.".to_string()),
        None,
        servers,
    ));
    parts.push(add_mcp);
    parts.push(form::group(
        "ACP agents",
        agents
            .is_empty()
            .then(|| "None yet. An agent answers in Ask in clew's place.".to_string()),
        None,
        agents,
    ));
    parts.push(form::group("Add an ACP agent", None, None, add_acp));
    parts.push(form::note(format!("More options in {}", crate::llm::config_hint())).into());
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
pub(crate) fn connect_modal(app: &App) -> Element<'_, Message> {
    use crate::ConnectStage;
    let Some(ui) = &app.connect else {
        return slot();
    };

    let title = row![
        glyph::icon(Glyph::Remote, theme::accent(), 18.0),
        text("Connect to Remote").size(ts::TITLE).color(theme::fg()),
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
                text(format!("Connecting to {label}…"))
                    .size(ts::BASE)
                    .color(theme::fg()),
                text("Preparing the host…")
                    .size(ts::SMALL)
                    .color(theme::dim()),
                space().height(6),
                button(text("Cancel").size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([4, 14])
                    .on_press(Message::Connect(ConnectMsg::Close)),
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
    let panel = container(column![title, keys, body].spacing(14))
        .width(DIALOG_W)
        .max_height(DIALOG_MAX_H)
        .padding(MODAL_PAD)
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
    let para = |s: String| {
        text(s)
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
    };
    let fingerprint = container(
        column![
            text(format!("{} key", key.kind))
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
            text("Unknown host key")
                .size(ts::SUBTITLE)
                .color(theme::warning()),
        ]
        .spacing(8)
        .align_y(iced::Center),
        para(format!(
            "First connection to {host}. Trust it only if this matches the host's key."
        )),
        fingerprint,
        row![
            space().width(Fill),
            button(text("Cancel").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([6, 14])
                .on_press(Message::Connect(ConnectMsg::TrustCancel)),
            crate::ui::fresh::allow(
                ready,
                button(text("Trust and connect").size(ts::BODY))
                    .style(theme::primary_button)
                    .padding([6, 14])
                    .on_press_maybe(ready.then(|| {
                        Message::Connect(ConnectMsg::TrustHost {
                            fingerprint: key.fingerprint.clone(),
                        })
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
    let para = |s: String| {
        text(s)
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
    };
    let prompt = column![
        row![
            glyph::icon(Glyph::Remote, theme::danger(), 16.0),
            text("Host key changed")
                .size(ts::SUBTITLE)
                .color(theme::danger()),
        ]
        .spacing(8)
        .align_y(iced::Center),
        para(reason.to_string()),
        para(format!(
            "Forget the old key for {forget_host} and reconnect to {}? Only if you know why it changed.",
            target.label()
        )),
        row![
            space().width(Fill),
            button(text("Cancel").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([6, 14])
                .on_press(Message::Connect(ConnectMsg::TrustCancel)),
            crate::ui::fresh::allow(
                ready,
                button(text("Forget the old key").size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([6, 14])
                    .on_press_maybe(ready.then(|| {
                        Message::Connect(ConnectMsg::ForgetHostKey {
                            host: forget_host.to_string(),
                        })
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
    let host = app.connection.label();
    let row: Element<'_, Message> = if held {
        row![
            glyph::icon(Glyph::Remote, theme::warning(), 14.0),
            text(format!("{host} holds your AI keys."))
                .size(ts::SMALL)
                .color(theme::fg()),
            space().width(Fill),
            button(text("Revoke").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([3, 10])
                .on_press(Message::Connect(ConnectMsg::RevokeAiKeys)),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into()
    } else {
        text(format!("{host} does not hold your AI keys."))
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

    // What comes before the form — the error, the saved hosts — in a place
    // of its own, there or not: pushed in front of the form's fields, it
    // handed their state on as it came and went (a field focused when Enter
    // showed an error lost its focus; a host saved from another window moved
    // the User field's cursor to Name).
    let mut head = Column::new().spacing(12);

    if let Some(msg) = error {
        head = head.push(
            container(text(msg.to_string()).size(ts::BODY).color(theme::danger()))
                .padding([6, 10])
                .width(Fill)
                .style(theme::modal_panel),
        );
    }

    // Saved hosts: click a row to connect, × to forget.
    if !app.saved_connections.is_empty() {
        head = head.push(section_header("SAVED HOSTS"));
        let mut list = Column::new().spacing(2);
        for conn in &app.saved_connections {
            // By identity: the list is re-merged from disk whenever any
            // window saves a host, so a row's index can name another host.
            let (user_host, port) = (conn.user_host(), conn.port);
            let open = button(
                row![
                    glyph::icon(Glyph::Remote, theme::fg_muted(), 14.0),
                    column![
                        text(conn.label()).size(ts::BASE).color(theme::fg()),
                        text(conn.user_host()).size(ts::SMALL).color(theme::dim()),
                    ]
                    .spacing(1),
                ]
                .spacing(8)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 10])
            .on_press(Message::Connect(ConnectMsg::ToSaved {
                user_host: user_host.clone(),
                port,
            }));
            let remove = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
                .style(theme::toolbar_button)
                .padding([5, 8])
                .on_press(Message::Connect(ConnectMsg::RemoveSaved {
                    user_host,
                    port,
                }));
            list = list.push(row![open, remove].spacing(4).align_y(iced::Center));
        }
        head = head.push(list);
    }
    let has_head = error.is_some() || !app.saved_connections.is_empty();
    let head =
        container(head).padding(Padding::default().bottom(if has_head { 12.0 } else { 0.0 }));
    let mut col = Column::new().spacing(12);

    // New-connection form. Enter in any field connects, like the button.
    let field = |title: &str, input: Element<'a, Message>| column![label(title), input].spacing(3);
    let input = |placeholder: &str, value: &str, f: ConnectField| {
        text_input(placeholder, value)
            .style(theme::input)
            .on_input(move |s| Message::Connect(ConnectMsg::Field(f, s)))
            .on_submit(Message::Connect(ConnectMsg::Submit))
            .size(ts::BASE)
            .padding(6)
    };

    let identity = row![
        input(
            "(optional) ~/.ssh/id_ed25519",
            &ui.identity,
            ConnectField::Identity
        )
        .width(Fill),
        form::action("Browse…", Some(Message::Connect(ConnectMsg::PickIdentity))),
    ]
    .spacing(6);

    col = col.push(section_header("NEW CONNECTION"));
    col = col.push(field(
        "Name (optional)",
        input("prod box", &ui.name, ConnectField::Name).into(),
    ));
    col = col.push(
        row![
            field(
                "Host",
                input("192.168.1.10 or example.com", &ui.host, ConnectField::Host).into()
            )
            .width(Fill),
            field("Port", input("22", &ui.port, ConnectField::Port).into()).width(80),
        ]
        .spacing(8),
    );
    col = col.push(field(
        "User",
        input("root", &ui.user, ConnectField::User).into(),
    ));
    col = col.push(field("Identity file", identity.into()));
    // Per-host consent for AI keys. Off: keys stay on this machine and AI
    // calls run locally. On: this host's clew-server receives the keys and
    // runs Ask/Explain remotely.
    col = col.push(
        iced::widget::checkbox(ui.send_ai_keys)
            .style(theme::check)
            .label("Send my AI keys to this host")
            .on_toggle(|v| Message::Connect(ConnectMsg::ToggleAiKeys(v)))
            .size(14)
            .text_size(ts::BODY)
            .spacing(8),
    );
    col = col.push(
        row![
            space().width(Fill),
            button(text("Connect").size(ts::BASE))
                .style(theme::primary_button)
                .padding([6, 18])
                .on_press(Message::Connect(ConnectMsg::Submit)),
        ]
        .align_y(iced::Center),
    );

    // While connected to a remote, offer a way back to local reading.
    if app.connection.is_remote() {
        col = col.push(row![
            space().width(Fill),
            button(text("Disconnect (read local code)").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::Connect(ConnectMsg::Disconnect)),
        ]);
    }

    scrollable(column![head, col.width(Fill)].width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Shrink)
        .into()
}

/// The line closing a folder listing the server cut at its cap: how many
/// entries it left out (files — folders are listed first). `None` for a
/// complete listing.
pub(crate) fn omitted_note(omitted: usize) -> Option<String> {
    match omitted {
        0 => None,
        1 => Some("… and 1 more".into()),
        n => Some(format!("… and {n} more")),
    }
}

/// The browsing stage: a path bar with an "up" control, the directory's contents
/// (folders navigable, files dimmed for context), and "Open this folder".
pub(crate) fn remote_browser_view(browser: &crate::RemoteBrowser) -> Element<'_, Message> {
    let mut up = button(glyph::icon(Glyph::ArrowLeft, theme::fg_muted(), 14.0))
        .style(theme::toolbar_button)
        .padding([4, 10]);
    if let Some(parent) = &browser.parent {
        up = up.on_press(Message::Connect(ConnectMsg::BrowseTo(parent.clone())));
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
            "Loading…"
        } else {
            "Empty folder."
        };
        rows.push(
            container(text(msg).size(ts::BODY).color(theme::dim()))
                .padding([4, 8])
                .into(),
        );
    }
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
                .on_press(Message::Connect(ConnectMsg::BrowseTo(remote_join(
                    &browser.cwd,
                    &entry.name,
                ))))
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
            text("Open this folder as the project")
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
        button(text("Open").size(ts::BASE))
            .style(theme::primary_button)
            .padding([6, 18])
            .on_press(Message::Connect(ConnectMsg::OpenHere)),
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
        None => form::note("Click a shortcut, then press new keys.").into(),
    };
    // Rebindable command rows.
    let mut cmds = Vec::new();
    for action in Action::ALL {
        let binding: Element<'_, Message> = if app.rebinding == Some(action) {
            container(
                text("Press a shortcut… esc to cancel")
                    .size(ts::BODY)
                    .color(theme::accent()),
            )
            .padding([3, 8])
            .into()
        } else {
            let pill = button(
                text(app.keymap.chord(action).caps())
                    .size(ts::BODY)
                    .color(theme::fg()),
            )
            .style(theme::secondary_button)
            .padding([3, 10])
            .on_press(Message::Window(WindowMsg::RebindStart(action)));
            if app.keymap.is_overridden(action) {
                row![
                    button(text("↺").size(ts::BASE).color(theme::dim()))
                        .style(theme::toolbar_button)
                        .padding([2, 7])
                        .on_press(Message::Window(WindowMsg::RebindReset(action))),
                    pill,
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into()
            } else {
                pill.into()
            }
        };
        cmds.push(slim(action.label().to_string(), binding));
    }

    // Read-only reading motions (not part of the customizable keymap).
    let motions: [(&str, &str); 16] = [
        ("Move left / down / up / right", "h j k l   ← ↓ ↑ →"),
        ("Word forward / back", "w   b"),
        ("Line start / end", "0   $"),
        ("File start / end", "gg   G"),
        ("Go to definition", "gd"),
        ("Find references", "gr"),
        ("Go to implementation", "gi"),
        ("Go to type definition", "gy"),
        ("Call hierarchy", "gc"),
        ("Expand macro / show goal / show type", "ge"),
        ("Toggle fold", "za"),
        ("Open all folds", "zR"),
        ("Close all folds", "zM"),
        ("Time Travel: older / newer commit", "⌘←   ⌘→"),
        ("Next / previous field", "tab   ⇧tab"),
        ("Close the top panel / clear selection", "esc"),
    ];
    let vim = motions
        .into_iter()
        .map(|(label, keys)| {
            slim(
                label.to_string(),
                text(crate::keymap::local_keys(keys))
                    .size(ts::BODY)
                    .font(crate::fonts::CODE)
                    .color(theme::fg_muted())
                    .into(),
            )
        })
        .collect();

    form::page(vec![
        notice,
        form::group("Commands", None, None, cmds),
        form::group("Reading motions", Some("Vim's, fixed.".into()), None, vim),
        form::note(format!("Saved to {}", crate::llm::config_hint())).into(),
    ])
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
