//! Settings, connect, remote-browser and shortcuts modals; explain-node helpers.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
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
pub(crate) fn settings_modal(app: &App) -> Element<'_, Message> {
    use crate::llm::Provider;
    let label = |s: &str| text(s.to_string()).size(ts::SMALL).color(theme::dim());
    let field =
        |title: &str, input: Element<'static, Message>| column![label(title), input].spacing(3);
    // Every text field submits the form on Enter.
    let input = |placeholder: &str, value: &str, on_input: fn(String) -> Message| {
        text_input(placeholder, value)
            .on_input(on_input)
            .on_submit(Message::Settings(SettingsMsg::Saved))
            .size(ts::BASE)
            .padding(6)
    };

    let provider: Element<'_, Message> =
        pick_list(&Provider::ALL[..], Some(app.settings.provider), |v| {
            Message::Settings(SettingsMsg::ProviderPicked(v))
        })
        .text_size(ts::BASE)
        .padding([4, 8])
        // Match the full width of the text fields below it, so the form column
        // doesn't look ragged with a half-width dropdown.
        .width(Fill)
        .into();

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

    let section = |s: &str| text(s.to_string()).size(ts::BODY).color(theme::accent());

    // A small segmented control: System / Light / Dark, active one filled.
    let theme_btn = |p: crate::theme::ThemePref| -> Element<'_, Message> {
        let active = app.theme_pref == p;
        let style: fn(&iced::Theme, iced::widget::button::Status) -> iced::widget::button::Style =
            if active {
                theme::primary_button
            } else {
                theme::secondary_button
            };
        button(text(p.label()).size(ts::BODY))
            .style(style)
            .padding([4, 16])
            .on_press(Message::Settings(SettingsMsg::SetThemePref(p)))
            .into()
    };
    let appearance = iced::widget::row(crate::theme::ThemePref::ALL.map(theme_btn)).spacing(6);

    // Per-mode theme pickers: which theme fills the light slot, and the dark.
    use crate::theme::ThemeChoice;
    let light_pick: Element<'_, Message> = pick_list(
        crate::theme::light_choices(),
        Some(ThemeChoice(crate::theme::current_light())),
        |c: ThemeChoice| {
            Message::Settings(SettingsMsg::SetThemeVariant {
                id: c.0.id,
                is_light: true,
            })
        },
    )
    .text_size(ts::BASE)
    .padding([4, 8])
    .width(Fill)
    .into();
    let dark_pick: Element<'_, Message> = pick_list(
        crate::theme::dark_choices(),
        Some(ThemeChoice(crate::theme::current_dark())),
        |c: ThemeChoice| {
            Message::Settings(SettingsMsg::SetThemeVariant {
                id: c.0.id,
                is_light: false,
            })
        },
    )
    .text_size(ts::BASE)
    .padding([4, 8])
    .width(Fill)
    .into();

    let header = row![
        text("Settings").size(ts::TITLE).color(theme::fg()),
        space().width(Fill),
        button(text("Save").size(ts::BODY))
            .style(theme::primary_button)
            .padding([3, 14])
            .on_press(Message::Settings(SettingsMsg::Saved)),
        button(text("Close").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::Settings(SettingsMsg::Close)),
    ]
    .spacing(6)
    .align_y(iced::Center);

    let form = column![
        section("Appearance"),
        appearance,
        field("Light theme", light_pick),
        field("Dark theme", dark_pick),
        section("Updates"),
        iced::widget::checkbox(app.update.auto_check)
            .label("Check for updates automatically")
            .on_toggle(|v| Message::Updater(UpdaterMsg::SetAuto(v)))
            .text_size(ts::BODY)
            .size(16)
            .spacing(8),
        text(format!(
            "You have clew {}",
            crate::updater::current_version()
        ))
        .size(ts::CAPTION)
        .color(theme::dim()),
        section("Language model"),
        field("Provider", provider),
        field("API key", key.into()),
        field("Model", model.into()),
        field("Base URL", base.into()),
        section("Embeddings (semantic search)"),
        field("API key", embed_key.into()),
        field("Model", embed_model.into()),
        field("Base URL", embed_base.into()),
        text(format!("Stored in {}", crate::llm::config_hint()))
            .size(ts::CAPTION)
            .color(theme::dim()),
        integrations_section(app),
        languages_section(app),
    ]
    .spacing(12)
    // Room for the scrollbar, so it never draws over a field's edge.
    .padding(Padding {
        right: SCROLLBAR_W + 6.0,
        ..Padding::ZERO
    });

    let panel = container(
        column![
            header,
            // Shrink: as tall as the form while it fits the window, then it
            // scrolls — the panel itself is bounded by the modal's margins.
            scrollable(form)
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Length::Shrink),
        ]
        .spacing(12),
    )
    .width(SETTINGS_W)
    .padding(MODAL_PAD)
    .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Settings(SettingsMsg::Close))),
    )
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

fn lang_caption<'a>(s: String) -> iced::widget::Text<'a> {
    text(s).size(ts::CAPTION).color(theme::dim())
}

fn lang_heading<'a>(s: &str) -> iced::widget::Text<'a> {
    text(s.to_string()).size(ts::BODY).color(theme::fg())
}

fn lang_button<'a>(label: &str, msg: Option<Message>) -> iced::widget::Button<'a, Message> {
    button(text(label.to_string()).size(ts::SMALL))
        .style(theme::secondary_button)
        .padding([2, 8])
        .on_press_maybe(msg)
}

/// A choice that is on or off, as a toggle.
fn lang_chip<'a>(label: String, on: bool, msg: Message) -> iced::widget::Button<'a, Message> {
    button(text(label).size(ts::SMALL))
        .style(theme::tab_button(on))
        .padding([2, 8])
        .on_press(msg)
}

fn lang_path<'a>(path: String) -> iced::widget::Text<'a> {
    text(path)
        .size(ts::CAPTION)
        .font(iced::Font::MONOSPACE)
        .color(theme::fg_muted())
        .wrapping(Wrapping::Glyph)
}

/// What is in effect, set apart.
fn lang_effect<'a>(lines: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    container(iced::widget::Column::with_children(lines).spacing(2))
        .padding([6, 8])
        .width(Fill)
        .style(theme::panel)
        .into()
}

/// A typed field of the section: Enter uses it.
fn lang_field<'a>(
    app: &'a App,
    placeholder: &str,
    field: crate::LangField,
) -> iced::widget::TextInput<'a, Message> {
    use crate::LanguagesMsg as L;
    text_input(placeholder, app.proj.lang_drafts.field(field))
        .on_input(move |v| Message::Languages(L::Draft(field, v)))
        .on_submit(Message::Languages(L::UseDraft(field)))
        .size(ts::BASE)
        .padding(6)
        .width(Fill)
}

/// A choice from a list: its label and detail, then "in use" or "Use".
fn lang_pick<'a>(
    label: String,
    detail: Element<'a, Message>,
    selected: bool,
    msg: Message,
) -> Element<'a, Message> {
    row![
        column![text(label).size(ts::SMALL).color(theme::fg()), detail]
            .spacing(1)
            .width(Fill),
        if selected {
            Element::from(text("in use").size(ts::SMALL).color(theme::success()))
        } else {
            lang_button("Use", Some(msg)).into()
        },
    ]
    .spacing(8)
    .align_y(iced::Center)
    .into()
}

/// What the project's `lsp.toml` sets that a choice here also sets.
fn project_sets_note<'a>(sets: &[String]) -> Option<Element<'a, Message>> {
    (!sets.is_empty()).then(|| {
        lang_caption(format!(
            "The project's .clew/lsp.toml sets {} — a choice here wins where both say.",
            sets.join(", ")
        ))
        .into()
    })
}

/// The open project's languages: each one's server and how it stands, and
/// what the user chooses for them — the Python interpreter, the Rust
/// features and target, the Go build tags and platform, the C/C++
/// compilation database and flags, Lean's and Dart's toolchains, a server
/// program of their own — each showing what is in effect before what there
/// is to pick, from what was found where the project is
/// (`App::refresh_language_options`). A pick is saved at once, for this
/// project on this machine, and never written into the project.
pub(crate) fn languages_section(app: &App) -> Element<'_, Message> {
    if app.proj.project.is_none() {
        return space().height(0).into();
    }
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let mut col = column![
        text("Languages in this project")
            .size(ts::BODY)
            .color(theme::accent()),
        lang_caption(format!(
            "Set up for this project {} — kept in clew's own data, never written into the \
             project. A choice here wins over its .clew/lsp.toml, and the code its build then \
             leaves out — features off, tags unset, macros undefined — is dimmed as you read.",
            if remote {
                "and its host"
            } else {
                "on this machine"
            }
        )),
    ]
    .spacing(8);
    let mut rows = app.proj.project_languages.clone();
    if let Some(servers) = &choices.servers {
        rows.extend(servers.keys().cloned());
    }
    rows.sort();
    rows.dedup();
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
            Some((label, msg)) => lang_button(label, Some(msg)).into(),
            None => space().width(0).into(),
        };
        col = col.push(
            row![
                text(
                    crate::highlight::lang_name(language)
                        .map(str::to_string)
                        .unwrap_or_else(|| language.clone())
                )
                .size(ts::BODY)
                .color(theme::fg())
                .width(90),
                column![
                    text(server)
                        .size(ts::SMALL)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::Glyph),
                    text(status).size(ts::CAPTION).color(theme::dim()),
                ]
                .spacing(1)
                .width(Fill),
                action,
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
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
    let options = match &app.proj.lang_options {
        Some(Ok(options)) => Some(options),
        Some(Err(why)) => {
            col = col.push(
                text(format!(
                    "What the project offers could not be listed: {why}"
                ))
                .size(ts::CAPTION)
                .color(theme::danger()),
            );
            None
        }
        None => {
            if listed {
                col = col.push(lang_caption("Looking at what the project offers…".into()));
            }
            None
        }
    };
    if python {
        col = col.push(python_block(app, options.and_then(|o| o.python.as_ref())));
    }
    if has("rust") {
        col = col.push(rust_block(app, options.and_then(|o| o.rust.as_ref())));
    }
    if has("go") {
        col = col.push(go_block(app, options.and_then(|o| o.go.as_ref())));
    }
    if has("c") || has("cpp") {
        col = col.push(c_block(app, options.and_then(|o| o.c.as_ref())));
    }
    for language in ["lean", "dart", "ruby", "erlang", "haskell"] {
        if has(language) {
            let found = options
                .and_then(|o| o.toolchains.get(language))
                .map(Vec::as_slice)
                .unwrap_or_default();
            col = col.push(toolchain_block(app, language, found));
        }
    }
    let found = |language: &str| {
        options
            .and_then(|o| o.toolchains.get(language))
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    if has("typescript") || has("tsx") || has("javascript") {
        col = col.push(path_choice_block(
            app,
            PathChoice {
                heading: "TypeScript and JavaScript: TypeScript version",
                automatic: "clew's own TypeScript 5, installed with the server",
                chosen: app.proj.lang_choices.typescript.as_deref(),
                found: found("typescript"),
                field: crate::LangField::TypeScriptPath,
                placeholder: "Path to a tsserver.js, or the TypeScript folder holding it",
                project_note: Some(
                    "This is the project's own TypeScript: the server runs its code — \
                     choose it for a project you trust.",
                ),
            },
            |c, path| c.typescript = path,
        ));
    }
    if has("java") {
        col = col.push(path_choice_block(
            app,
            PathChoice {
                heading: "Java: JDK",
                automatic: "The JDK the server finds (JAVA_HOME, or the one on the PATH)",
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
        col = col.push(path_choice_block(
            app,
            PathChoice {
                heading: "PHP: version",
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
        col = col.push(path_choice_block(
            app,
            PathChoice {
                heading: "C#: solution",
                automatic: "The solution the server finds in the project",
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
        col = col.push(path_choice_block(
            app,
            PathChoice {
                heading: "Zig: compiler",
                automatic: "The zig zls finds on the PATH",
                chosen: app.proj.lang_choices.zig.as_deref(),
                found: found("zig"),
                field: crate::LangField::ZigPath,
                placeholder: "Path to a zig compiler, or a name on the PATH",
                project_note: None,
            },
            |c, path| c.zig = path,
        ));
    }
    col = col.push(servers_block(app));
    if listed {
        col = col.push(
            row![
                space().width(Fill),
                lang_button(
                    "Look again",
                    Some(Message::Languages(crate::LanguagesMsg::Refresh))
                ),
            ]
            .align_y(iced::Center),
        );
    }
    col.into()
}

/// The Python interpreter: the one in effect and who named it, the ones
/// found, and a path of the user's own.
fn python_block<'a>(
    app: &'a App,
    envs: Option<&'a clew_protocol::PythonEnvironments>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    use clew_protocol::{PythonEnvKind, PythonSource};
    let remote = app.connection.is_remote();
    let chosen = app.proj.lang_choices.python.as_deref();
    let mut col = column![lang_heading("Python interpreter")].spacing(6);
    if let Some(envs) = envs {
        let effect = &envs.in_effect;
        let who = match effect.source {
            PythonSource::Chosen => "Your choice",
            PythonSource::LspToml => "Named by the project's .clew/lsp.toml",
            PythonSource::ProjectVenv => {
                "Found in the project — run through its base Python, its packages read, \
                 as it came with the project"
            }
            PythonSource::Activated => "The environment active where clew started",
            PythonSource::Server => "None — the language server looks for one itself",
        };
        let mut lines: Vec<Element<'a, Message>> =
            vec![text(who).size(ts::SMALL).color(theme::fg()).into()];
        if let Some(path) = &effect.interpreter {
            lines.push(lang_path(path.clone()).into());
        }
        col = col.push(lang_effect(lines));
        if let Some(bad) = &effect.unusable_choice {
            col = col.push(
                text(format!(
                    "Your choice {bad} cannot be used: it is gone, or it is a program of \
                     the repository's own. The one above is used until you pick another."
                ))
                .size(ts::CAPTION)
                .color(theme::warning()),
            );
        }
        col = col.push(lang_pick(
            "Automatic".into(),
            lang_caption(
                "Whatever clew finds: the project's own venv, else the server's choice".into(),
            )
            .into(),
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
            col = col.push(lang_pick(
                label,
                lang_path(candidate.interpreter.clone()).into(),
                chosen == Some(candidate.interpreter.as_str()),
                Message::Languages(L::ChoosePython(Some(candidate.interpreter.clone()))),
            ));
        }
        // A choice the list does not hold (typed, browsed to, or gone).
        if let Some(other) = chosen
            && !envs.candidates.iter().any(|c| c.interpreter == other)
        {
            col = col.push(lang_pick(
                "Your choice".into(),
                lang_path(other.to_string()).into(),
                true,
                Message::Languages(L::ChoosePython(Some(other.to_string()))),
            ));
        }
    }
    let placeholder = if remote {
        "Path to a Python interpreter on the remote host"
    } else {
        "Path to a Python interpreter"
    };
    let draft_empty = app.proj.lang_drafts.python_path.trim().is_empty();
    let mut path_row = row![
        lang_field(app, placeholder, F::PythonPath),
        lang_button(
            "Use",
            (!draft_empty).then_some(Message::Languages(L::UseDraft(F::PythonPath)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        path_row = path_row.push(lang_button(
            "Browse…",
            Some(Message::Languages(L::Browse(F::PythonPath))),
        ));
    }
    col.push(path_row).into()
}

/// The Cargo features and target rust-analyzer loads the project with.
fn rust_block<'a>(
    app: &'a App,
    options: Option<&'a clew_protocol::RustOptions>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let rust = choices.rust.clone().unwrap_or_default();
    let mut col = column![lang_heading("Rust: features and target")].spacing(6);
    let features = if rust.all_features {
        "Every feature of every package".to_string()
    } else if rust.features.is_empty() {
        match rust.no_default_features {
            true => "No features at all".into(),
            false => "Each package's default features".into(),
        }
    } else {
        format!(
            "Features {}{}",
            rust.features.join(", "),
            match rust.no_default_features {
                true => ", without the defaults",
                false => ", and the defaults",
            }
        )
    };
    let target = match &rust.target {
        Some(target) => format!("Checked for {target}"),
        None => "Checked for the host's own target".into(),
    };
    col = col.push(lang_effect(vec![
        text(features).size(ts::SMALL).color(theme::fg()).into(),
        text(target).size(ts::SMALL).color(theme::fg()).into(),
    ]));
    if let Some(note) = options.and_then(|o| project_sets_note(&o.project_sets)) {
        col = col.push(note);
    }
    col = col.push(
        iced::widget::checkbox(rust.all_features)
            .label("Every feature of every package")
            .on_toggle(move |on| {
                choose_with(choices, |c| {
                    c.rust.get_or_insert_default().all_features = on
                })
            })
            .text_size(ts::SMALL)
            .size(14)
            .spacing(6),
    );
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
                    .size(ts::SMALL)
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
                chips.push(
                    lang_chip(label, rust.features.contains(&name), toggle(name.clone())).into(),
                );
                listed.push(name);
            }
            col = col.push(Row::with_children(chips).spacing(4).wrap());
        }
        // Chosen ones no package lists (any more): shown, to be turned off.
        let others: Vec<Element<'a, Message>> = rust
            .features
            .iter()
            .filter(|f| !listed.contains(f))
            .map(|f| lang_chip(f.clone(), true, toggle(f.clone())).into())
            .collect();
        if !others.is_empty() {
            col = col.push(Row::with_children(others).spacing(4).wrap());
        }
        col = col.push(
            iced::widget::checkbox(rust.no_default_features)
                .label("Without the packages' default features")
                .on_toggle(move |on| {
                    choose_with(choices, |c| {
                        c.rust.get_or_insert_default().no_default_features = on
                    })
                })
                .text_size(ts::SMALL)
                .size(14)
                .spacing(6),
        );
    }
    let mut targets = vec![HOST_CHOICE.to_string()];
    targets.extend(options.into_iter().flat_map(|o| o.targets.iter().cloned()));
    targets.extend(RUST_TARGETS.iter().map(|t| t.to_string()));
    targets.extend(rust.target.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    targets.retain(|t| seen.insert(t.clone()));
    let selected = rust.target.clone().unwrap_or_else(|| HOST_CHOICE.into());
    col.push(
        row![
            text("Target").size(ts::SMALL).color(theme::fg()),
            pick_list(targets, Some(selected), move |t: String| {
                choose_with(choices, |c| {
                    c.rust.get_or_insert_default().target = (t != HOST_CHOICE).then_some(t)
                })
            })
            .text_size(ts::SMALL)
            .padding([2, 6]),
            lang_field(app, "Another target triple", F::RustTarget),
            lang_button(
                "Use",
                (!app.proj.lang_drafts.rust_target.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::RustTarget)))
            ),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .into()
}

/// The build tags and platform gopls loads the project with.
fn go_block<'a>(
    app: &'a App,
    options: Option<&'a clew_protocol::GoOptions>,
) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let go = choices.go.clone().unwrap_or_default();
    let mut col = column![lang_heading("Go: build tags and platform")].spacing(6);
    let tags = match go.tags.is_empty() {
        true => "No build tags".to_string(),
        false => format!("Build tags {}", go.tags.join(", ")),
    };
    let platform = match (&go.goos, &go.goarch) {
        (None, None) => "Read for the host's own platform".to_string(),
        (os, arch) => format!(
            "Read for {}/{}",
            os.as_deref().unwrap_or("the host's system"),
            arch.as_deref().unwrap_or("the host's architecture")
        ),
    };
    col = col.push(lang_effect(vec![
        text(tags).size(ts::SMALL).color(theme::fg()).into(),
        text(platform).size(ts::SMALL).color(theme::fg()).into(),
    ]));
    if let Some(note) = options.and_then(|o| project_sets_note(&o.project_sets)) {
        col = col.push(note);
    }
    let mut all: Vec<String> = options.map(|o| o.tags.clone()).unwrap_or_default();
    all.extend(go.tags.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    all.retain(|t| seen.insert(t.clone()));
    if all.is_empty() {
        col = col.push(lang_caption(
            "The project's files name no build tags of their own.".into(),
        ));
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
                lang_chip(tag, on, msg).into()
            })
            .collect();
        col = col.push(Row::with_children(chips).spacing(4).wrap());
    }
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
                text(label).size(ts::SMALL).color(theme::fg()),
                pick_list(list, Some(selected), move |v: String| {
                    choose_with(choices, |c| {
                        set(
                            c.go.get_or_insert_default(),
                            (v != HOST_CHOICE).then_some(v),
                        )
                    })
                })
                .text_size(ts::SMALL)
                .padding([2, 6]),
            ]
            .spacing(6)
            .align_y(iced::Center)
        };
    col.push(
        row![
            lang_field(app, "Another build tag", F::GoTag),
            lang_button(
                "Add",
                (!app.proj.lang_drafts.go_tag.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::GoTag)))
            ),
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
        .spacing(6)
        .align_y(iced::Center),
    )
    .into()
}

/// The compilation database and flags clangd compiles C and C++ with.
fn c_block<'a>(app: &'a App, options: Option<&'a clew_protocol::COptions>) -> Element<'a, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let c = choices.c.clone().unwrap_or_default();
    let mut col = column![lang_heading("C and C++: compilation")].spacing(6);
    let database: Element<'a, Message> = match &c.compile_commands {
        Some(dir) => column![
            text("Compilation database: your choice")
                .size(ts::SMALL)
                .color(theme::fg()),
            lang_path(dir.clone()),
        ]
        .into(),
        None => text("Compilation database: the one clangd finds beside the sources or in build/")
            .size(ts::SMALL)
            .color(theme::fg())
            .into(),
    };
    let flags = match c.flags.is_empty() {
        true => "No flags for files it does not cover".to_string(),
        false => format!(
            "For files it does not cover: {}",
            crate::app::languages::join_args(&c.flags)
        ),
    };
    col = col.push(lang_effect(vec![
        database,
        text(flags).size(ts::SMALL).color(theme::fg()).into(),
    ]));
    if let Some(note) = options.and_then(|o| project_sets_note(&o.project_sets)) {
        col = col.push(note);
    }
    col = col.push(lang_pick(
        "Automatic".into(),
        lang_caption("clangd looks beside each file, and in build/".into()).into(),
        c.compile_commands.is_none(),
        choose_with(choices, |c| {
            c.c.get_or_insert_default().compile_commands = None
        }),
    ));
    let found = options.map(|o| o.databases.as_slice()).unwrap_or_default();
    for dir in found {
        col = col.push(lang_pick(
            "compile_commands.json".into(),
            lang_path(dir.clone()).into(),
            c.compile_commands.as_deref() == Some(dir.as_str()),
            choose_with(choices, |c| {
                c.c.get_or_insert_default().compile_commands = Some(dir.clone())
            }),
        ));
    }
    let mut path_row = row![
        lang_field(
            app,
            if remote {
                "Folder holding compile_commands.json, on the remote host"
            } else {
                "Folder holding compile_commands.json"
            },
            F::CDatabase
        ),
        lang_button(
            "Use",
            (!app.proj.lang_drafts.c_database.trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(F::CDatabase)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        path_row = path_row.push(lang_button(
            "Browse…",
            Some(Message::Languages(L::Browse(F::CDatabase))),
        ));
    }
    col = col.push(path_row);
    let mut flags_row = row![
        lang_field(app, "Flags, e.g. -DDEBUG -Iinclude -std=c++20", F::CFlags),
        lang_button(
            "Use",
            (!app.proj.lang_drafts.c_flags.trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(F::CFlags)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !c.flags.is_empty() {
        flags_row = flags_row.push(lang_button(
            "Clear",
            Some(choose_with(choices, |c| {
                c.c.get_or_insert_default().flags.clear()
            })),
        ));
    }
    col.push(flags_row).into()
}

/// A path chosen for a language from what was found, or typed: the
/// TypeScript its server runs, the `zig` zls uses.
struct PathChoice<'a> {
    heading: &'static str,
    /// What is used when nothing is chosen.
    automatic: &'static str,
    chosen: Option<&'a str>,
    found: &'a [clew_protocol::ToolchainCandidate],
    field: crate::LangField,
    placeholder: &'static str,
    /// Said when the one chosen is the project's own.
    project_note: Option<&'static str>,
}

/// A [`PathChoice`]: what is in effect, "Automatic", what was found, and a
/// typed path — `set` making the choice in a copy of the choices.
fn path_choice_block<'a>(
    app: &'a App,
    choice: PathChoice<'a>,
    set: fn(&mut clew_protocol::LanguageChoices, Option<String>),
) -> Element<'a, Message> {
    use crate::LanguagesMsg as L;
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let mut col = column![lang_heading(choice.heading)].spacing(6);
    let effect: Vec<Element<'a, Message>> = match choice.chosen {
        Some(path) => vec![
            text("Your choice")
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
            lang_path(path.to_string()).into(),
        ],
        None => vec![
            text(choice.automatic)
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
        ],
    };
    col = col.push(lang_effect(effect));
    let chosen_is_projects = choice
        .found
        .iter()
        .any(|c| c.project && Some(c.command.as_str()) == choice.chosen);
    if let Some(note) = choice.project_note.filter(|_| chosen_is_projects) {
        col = col.push(text(note).size(ts::CAPTION).color(theme::warning()));
    }
    col = col.push(lang_pick(
        "Automatic".into(),
        lang_caption(choice.automatic.into()).into(),
        choice.chosen.is_none(),
        choose_with(choices, |c| set(c, None)),
    ));
    for candidate in choice.found {
        col = col.push(lang_pick(
            candidate.name.clone(),
            lang_path(candidate.command.clone()).into(),
            choice.chosen == Some(candidate.command.as_str()),
            choose_with(choices, |c| set(c, Some(candidate.command.clone()))),
        ));
    }
    if let Some(other) = choice.chosen
        && !choice.found.iter().any(|c| c.command == other)
    {
        col = col.push(lang_pick(
            "Your choice".into(),
            lang_path(other.to_string()).into(),
            true,
            choose_with(choices, |c| set(c, Some(other.to_string()))),
        ));
    }
    let field = choice.field;
    let mut path_row = row![
        lang_field(app, choice.placeholder, field),
        lang_button(
            "Use",
            (!app.proj.lang_drafts.field(field).trim().is_empty())
                .then_some(Message::Languages(L::UseDraft(field)))
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        path_row = path_row.push(lang_button(
            "Browse…",
            Some(Message::Languages(L::Browse(field))),
        ));
    }
    col.push(path_row).into()
}

/// Where the toolchain `language`'s server is run from (Lean's `lake`,
/// Dart's `dart`): the ones found, and clew's own lookup on `PATH`.
fn toolchain_block<'a>(
    app: &'a App,
    language: &'static str,
    found: &'a [clew_protocol::ToolchainCandidate],
) -> Element<'a, Message> {
    let choices = &app.proj.lang_choices;
    let (heading, program) = match language {
        "lean" => ("Lean: toolchain", "lake"),
        "ruby" => ("Ruby: the Ruby whose Ruby LSP runs", "ruby-lsp"),
        "erlang" => ("Erlang: language platform", "elp"),
        "haskell" => (
            "Haskell: language server",
            "haskell-language-server-wrapper",
        ),
        _ => ("Dart: SDK", "dart"),
    };
    let chosen = choices.servers.as_ref().and_then(|s| s.get(language));
    let mut col = column![lang_heading(heading)].spacing(6);
    let effect: Vec<Element<'a, Message>> = match chosen {
        Some(choice) if choice.args.is_none() => vec![
            text("Your choice")
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
            lang_path(choice.command.clone()).into(),
        ],
        Some(choice) => vec![
            text("Your own server program (under Server programs)")
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
            lang_path(program_line(choice)).into(),
        ],
        None => vec![
            text(format!("The {program} found on the PATH"))
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
        ],
    };
    col = col.push(lang_effect(effect));
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
    col = col.push(lang_pick(
        "Automatic".into(),
        lang_caption(format!("The {program} on the PATH")).into(),
        chosen.is_none(),
        set(None),
    ));
    for candidate in found {
        let label = match candidate.project {
            true => format!("{} — the project's", candidate.name),
            false => candidate.name.clone(),
        };
        col = col.push(lang_pick(
            label,
            lang_path(candidate.command.clone()).into(),
            chosen.is_some_and(|c| c.args.is_none() && c.command == candidate.command),
            set(Some(candidate.command.clone())),
        ));
    }
    if found.is_empty() {
        col = col.push(lang_caption(format!(
            "No other {program} was found; give one's path under Server programs."
        )));
    }
    col.into()
}

/// The server programs the user chose, and the form to choose one — for a
/// language clew serves (another build, another server) or one it does not.
fn servers_block(app: &App) -> Element<'_, Message> {
    use crate::{LangField as F, LanguagesMsg as L};
    let choices = &app.proj.lang_choices;
    let remote = app.connection.is_remote();
    let drafts = &app.proj.lang_drafts;
    let mut col = column![
        lang_heading("Server programs"),
        lang_caption(
            "Run a program of your own as a language's server: another build, another \
             server, or one for a language clew has none for. With no arguments given, it \
             runs with the ones clew's server for the language takes. A server that builds \
             the project still asks before it does."
                .into(),
        ),
    ]
    .spacing(6);
    for (language, choice) in choices.servers.iter().flatten() {
        let name = crate::highlight::lang_name(language).unwrap_or(language.as_str());
        let remove = choose_with(choices, |c| {
            c.servers.get_or_insert_default().remove(language);
        });
        col = col.push(
            row![
                text(name.to_string())
                    .size(ts::SMALL)
                    .color(theme::fg())
                    .width(90),
                lang_path(program_line(choice)).width(Fill),
                lang_button(
                    "Edit",
                    Some(Message::Languages(L::ServerLanguage(language.clone())))
                ),
                lang_button("Reset", Some(remove)),
            ]
            .spacing(6)
            .align_y(iced::Center),
        );
    }
    let mut languages = app.proj.file_languages.clone();
    languages.extend(app.proj.project_languages.iter().cloned());
    languages.sort();
    languages.dedup();
    let ready = drafts.server_language.is_some();
    let mut program_row = row![
        pick_list(languages, drafts.server_language.clone(), |l: String| {
            Message::Languages(L::ServerLanguage(l))
        })
        .placeholder("Language")
        .text_size(ts::SMALL)
        .padding([4, 6]),
        lang_field(
            app,
            if remote {
                "Program on the remote host: a path, or a name on its PATH"
            } else {
                "Program: a path, or a name on the PATH"
            },
            F::ServerCommand
        ),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if !remote {
        program_row = program_row.push(lang_button(
            "Browse…",
            Some(Message::Languages(L::Browse(F::ServerCommand))),
        ));
    }
    col = col.push(program_row);
    col.push(
        row![
            lang_field(
                app,
                "Its arguments (blank: clew's for the language)",
                F::ServerArgs
            ),
            lang_button(
                "Use",
                (ready && !drafts.server_command.trim().is_empty())
                    .then_some(Message::Languages(L::UseDraft(F::ServerCommand)))
            ),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .into()
}

/// The settings' MCP servers and ACP agents: what is configured, how each
/// stands, and forms to add more. Every change is saved at once.
fn integrations_section(app: &App) -> Element<'_, Message> {
    use crate::IntegrationsMsg as M;
    use clew_core::mcp::hub::Status;
    let draft = &app.settings.integrations;
    let section = |s: &str| text(s.to_string()).size(ts::BODY).color(theme::accent());
    let caption = |s: String| text(s).size(ts::CAPTION).color(theme::dim());
    let small_button = |label: &str, msg: Message| {
        button(text(label.to_string()).size(ts::SMALL))
            .style(theme::secondary_button)
            .padding([2, 8])
            .on_press(msg)
    };
    let field =
        |placeholder: &str, value: &str, on_input: fn(String) -> Message, submit: Message| {
            text_input(placeholder, value)
                .on_input(on_input)
                .on_submit(submit)
                .size(ts::BASE)
                .padding(6)
        };

    let mut col = column![
        section("MCP servers"),
        caption(
            "Their tools join Ask's answers (with clew's own agent). Each call asks you first \
             unless the server is set otherwise."
                .into()
        ),
    ]
    .spacing(8);
    for server in &draft.mcp {
        let status: Element<'_, Message> = match draft.statuses.get(&server.name) {
            _ if !server.enabled => caption("off".into()).into(),
            Some(Status::Connected { info, tools }) => text(format!(
                "● connected{} · {tools} tools",
                if info.name.is_empty() {
                    String::new()
                } else {
                    format!(" to {}", info.name)
                }
            ))
            .size(ts::CAPTION)
            .color(theme::success())
            .into(),
            Some(Status::Connecting) => caption("connecting…".into()).into(),
            Some(Status::Failed(why)) => text(format!("✗ {why}"))
                .size(ts::CAPTION)
                .color(theme::danger())
                .into(),
            Some(Status::Idle) | None => caption("not connected yet".into()).into(),
        };
        let name = server.name.clone();
        let policy: Element<'_, Message> = pick_list(
            &clew_core::mcp::Approve::ALL[..],
            Some(server.approve),
            move |a| Message::Integrations(M::McpApprove(name.clone(), a)),
        )
        .text_size(ts::SMALL)
        .padding([2, 6])
        .into();
        // An HTTP server reached without a header of the user's signs in.
        let oauth = matches!(
            &server.transport,
            clew_core::mcp::Transport::Http { headers, .. }
                if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        );
        let sign_in: Element<'_, Message> = if !oauth {
            space().width(0).into()
        } else if draft.signed_in.contains(&server.name) {
            small_button(
                "Sign out",
                Message::Integrations(M::McpSignOut(server.name.clone())),
            )
            .into()
        } else {
            small_button(
                "Sign in",
                Message::Integrations(M::McpSignIn(server.name.clone())),
            )
            .into()
        };
        let name = server.name.clone();
        let enabled = iced::widget::checkbox(server.enabled)
            .label("On")
            .on_toggle(move |on| Message::Integrations(M::McpEnable(name.clone(), on)))
            .text_size(ts::SMALL)
            .size(14)
            .spacing(4);
        col = col.push(
            container(
                column![
                    row![
                        text(server.name.clone()).size(ts::BODY).color(theme::fg()),
                        space().width(Fill),
                        enabled,
                        policy,
                        sign_in,
                        small_button(
                            "Test",
                            Message::Integrations(M::McpTest(server.name.clone()))
                        ),
                        small_button(
                            "Remove",
                            Message::Integrations(M::McpRemove(server.name.clone()))
                        ),
                    ]
                    .spacing(6)
                    .align_y(iced::Center),
                    text(server.summary())
                        .size(ts::CAPTION)
                        .font(iced::Font::MONOSPACE)
                        .color(theme::fg_muted()),
                    status,
                ]
                .spacing(3),
            )
            .padding([6, 8])
            .width(Fill)
            .style(theme::panel),
        );
    }
    col = col.push(
        column![
            row![
                field(
                    "name",
                    &draft.new_mcp_name,
                    |v| Message::Integrations(M::McpName(v)),
                    Message::Integrations(M::McpAdd)
                )
                .width(Length::FillPortion(1)),
                field(
                    "command (npx -y …), https:// URL, or a {\"mcpServers\": …} snippet",
                    &draft.new_mcp_target,
                    |v| Message::Integrations(M::McpTarget(v)),
                    Message::Integrations(M::McpAdd),
                )
                .width(Length::FillPortion(3)),
            ]
            .spacing(6),
            row![
                field(
                    "env for a command, headers for a URL: KEY=value; Authorization: Bearer …",
                    &draft.new_mcp_extra,
                    |v| Message::Integrations(M::McpExtra(v)),
                    Message::Integrations(M::McpAdd),
                )
                .secure(true)
                .width(Fill),
                button(text("Add server").size(ts::SMALL))
                    .style(theme::primary_button)
                    .padding([5, 12])
                    .on_press(Message::Integrations(M::McpAdd)),
            ]
            .spacing(6)
            .align_y(iced::Center),
        ]
        .spacing(6),
    );

    col = col.push(section("ACP agents"));
    col = col.push(caption(
        "Coding agents that answer in the Ask panel instead of clew's own (pick one there). \
         They run on this machine, with clew's code tools; they ask before they act."
            .into(),
    ));
    for agent in &draft.acp {
        let name = agent.name.clone();
        let share = iced::widget::checkbox(agent.share_mcp)
            .label("Share my MCP servers")
            .on_toggle(move |on| Message::Integrations(M::AcpShareMcp(name.clone(), on)))
            .text_size(ts::SMALL)
            .size(14)
            .spacing(4);
        col = col.push(
            container(
                column![
                    row![
                        text(crate::app::integrations::acp_label(agent))
                            .size(ts::BODY)
                            .color(theme::fg()),
                        space().width(Fill),
                        share,
                        small_button(
                            "Remove",
                            Message::Integrations(M::AcpRemove(agent.name.clone()))
                        ),
                    ]
                    .spacing(6)
                    .align_y(iced::Center),
                    text(agent.summary())
                        .size(ts::CAPTION)
                        .font(iced::Font::MONOSPACE)
                        .color(theme::fg_muted()),
                ]
                .spacing(3),
            )
            .padding([6, 8])
            .width(Fill)
            .style(theme::panel),
        );
    }
    let presets: Vec<Element<'_, Message>> = clew_core::acp::config::PRESETS
        .iter()
        .enumerate()
        .filter(|(_, p)| !draft.acp.iter().any(|a| a.name == p.name))
        .map(|(i, p)| {
            button(text(format!("+ {}", p.label)).size(ts::SMALL))
                .style(theme::secondary_button)
                .padding([3, 10])
                .on_press(Message::Integrations(M::AcpPreset(i)))
                .into()
        })
        .collect();
    if !presets.is_empty() {
        col = col.push(Row::with_children(presets).spacing(6).wrap());
    }
    col = col.push(
        row![
            field(
                "name",
                &draft.new_acp_name,
                |v| Message::Integrations(M::AcpName(v)),
                Message::Integrations(M::AcpAdd)
            )
            .width(Length::FillPortion(1)),
            field(
                "command that starts it in ACP mode",
                &draft.new_acp_command,
                |v| Message::Integrations(M::AcpCommand(v)),
                Message::Integrations(M::AcpAdd),
            )
            .width(Length::FillPortion(3)),
            button(text("Add agent").size(ts::SMALL))
                .style(theme::primary_button)
                .padding([5, 12])
                .on_press(Message::Integrations(M::AcpAdd)),
        ]
        .spacing(6)
        .align_y(iced::Center),
    );
    if let Some(note) = &draft.note {
        col = col.push(text(note.clone()).size(ts::SMALL).color(theme::fg_muted()));
    }
    for error in &draft.errors {
        col = col.push(text(error.clone()).size(ts::SMALL).color(theme::danger()));
    }
    col = col.push(caption(format!(
        "Environment, timeouts and per-tool approvals: [mcp.servers.<name>] and \
         [acp.agents.<name>] in {}",
        crate::llm::config_hint()
    )));
    col.into()
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
        button(text("Close").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::Connect(ConnectMsg::Close)),
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
                text("Preparing the server on the remote host.")
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
    let where_to = match crate::connect::known_hosts_path() {
        Some(path) => format!(
            "Trusting records this one key in clew's own known_hosts ({}). Your \
             ~/.ssh/known_hosts is not changed.",
            path.display()
        ),
        None => "Trusting records this one key in clew's own known_hosts. Your \
                 ~/.ssh/known_hosts is not changed."
            .to_string(),
    };
    let fingerprint = container(
        column![
            text(format!("{} key", key.kind))
                .size(ts::CAPTION)
                .color(theme::dim()),
            text(key.fingerprint.as_str())
                .size(ts::BASE)
                .font(Font::MONOSPACE)
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
            "clew has never seen {host} before, so it cannot tell whether it is really \
             that host. It presented this key:"
        )),
        fingerprint,
        para(
            "Compare the fingerprint with one you trust — from the host's administrator, \
             or `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` run on the host. Trust \
             it only if they match."
                .to_string()
        ),
        text(where_to).size(ts::SMALL).color(theme::dim()),
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
            "Forgetting removes the key clew recorded for {forget_host} from clew's own \
             known_hosts (your ~/.ssh/known_hosts is not changed) and connects to {} \
             again. Its new key is then shown to be checked before it is trusted. Do \
             this only if you know why the key changed.",
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
            text(format!(
                "{host} holds your AI API keys — Ask and Explain run there."
            ))
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
        text(format!(
            "{host} does not hold your AI API keys — AI calls run on this Mac."
        ))
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
        button(text("Browse…").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([6, 12])
            .on_press(Message::Connect(ConnectMsg::PickIdentity)),
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
            .label("Send my AI API keys to this host (Ask/Explain run remotely)")
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
        1 => Some("… and 1 more entry, past the listing's cap".into()),
        n => Some(format!("… and {n} more entries, past the listing's cap")),
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
                .font(Font::MONOSPACE)
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
                .font(Font::MONOSPACE)
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

/// The "Keyboard Shortcuts" modal: rebindable command chords on top, the fixed
/// Vim-style reading motions below as a read-only reference.
pub(crate) fn shortcuts_modal(app: &App) -> Element<'_, Message> {
    use crate::keymap::Action;
    let section = |s: &str| text(s.to_string()).size(ts::BODY).color(theme::accent());

    // Header: title, optional "Reset all", Close.
    let mut header = row![
        text("Keyboard Shortcuts")
            .size(ts::TITLE)
            .color(theme::fg()),
        space().width(Fill)
    ]
    .spacing(6)
    .align_y(iced::Center);
    if app.keymap.any_overridden() {
        header = header.push(
            button(text("Reset all").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::Window(WindowMsg::RebindResetAll)),
        );
    }
    header = header.push(
        button(text("Close").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::Window(WindowMsg::CloseShortcuts)),
    );

    // A one-line hint, replaced by a warning when a rebind is rejected.
    let notice: Element<'_, Message> = match &app.keymap_notice {
        Some(msg) => text(msg.clone())
            .size(ts::SMALL)
            .color(theme::warning())
            .into(),
        None => text("Click a shortcut, then press the new keys. Esc cancels.")
            .size(ts::SMALL)
            .color(theme::dim())
            .into(),
    };

    // Rebindable command rows.
    let mut cmds = Column::new().spacing(2);
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
                    .size(ts::BASE)
                    .color(theme::fg()),
            )
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(Message::Window(WindowMsg::RebindStart(action)));
            if app.keymap.is_overridden(action) {
                row![
                    pill,
                    button(text("↺").size(ts::BASE).color(theme::dim()))
                        .style(theme::toolbar_button)
                        .padding([3, 7])
                        .on_press(Message::Window(WindowMsg::RebindReset(action))),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into()
            } else {
                pill.into()
            }
        };
        cmds = cmds.push(
            row![
                text(action.label()).size(ts::BASE).color(theme::fg()),
                space().width(Fill),
                binding,
            ]
            .align_y(iced::Center)
            .spacing(10)
            .padding([1, 2]),
        );
    }

    // Read-only reading motions (not part of the customizable keymap).
    let motions: [(&str, &str); 15] = [
        ("Move left / down / up / right", "h j k l   ← ↓ ↑ →"),
        ("Word forward / back", "w   b"),
        ("Line start / end", "0   $"),
        ("File start / end", "gg   G"),
        ("Go to definition", "gd"),
        ("Find references", "gr"),
        ("Go to implementation", "gi"),
        ("Go to type definition", "gy"),
        ("Call hierarchy", "gc"),
        ("Toggle fold", "za"),
        ("Open all folds", "zR"),
        ("Close all folds", "zM"),
        ("Time Travel: older / newer commit", "⌘←   ⌘→"),
        ("Next / previous field", "tab   ⇧tab"),
        ("Close the top panel / clear selection", "esc"),
    ];
    let mut vim = Column::new().spacing(2);
    for (label, keys) in motions {
        vim = vim.push(
            row![
                text(label).size(ts::BASE).color(theme::fg()),
                space().width(Fill),
                text(keys).size(ts::BODY).color(theme::dim()),
            ]
            .align_y(iced::Center)
            .spacing(10)
            .padding([1, 2]),
        );
    }

    let scroll_body = scrollable(
        column![
            section("Commands"),
            cmds,
            space().height(8),
            section("Reading motions (Vim, fixed)"),
            vim,
        ]
        .spacing(8)
        .width(Fill)
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 0.0,
            left: 0.0,
        }),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    // Fill what the panel leaves (at most its max height, less in a short
    // window) instead of a fixed height that a short window clipped.
    .height(Fill);

    let panel = container(
        column![
            header,
            notice,
            scroll_body,
            text(format!("Saved to {}", crate::llm::config_hint()))
                .size(ts::CAPTION)
                .color(theme::dim()),
        ]
        .spacing(12),
    )
    .width(SHORTCUTS_W)
    .max_height(SHORTCUTS_MAX_H)
    .padding(MODAL_PAD)
    .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Window(WindowMsg::CloseShortcuts))),
    )
}
