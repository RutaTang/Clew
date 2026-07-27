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
pub(crate) fn settings_modal(app: &App) -> Element<'_, Message> {
    use crate::llm::Provider;
    let label = |s: &str| text(s.to_string()).size(11).color(theme::dim());
    let field =
        |title: &str, input: Element<'static, Message>| column![label(title), input].spacing(3);

    let provider: Element<'_, Message> = pick_list(
        &Provider::ALL[..],
        Some(app.settings.provider),
        Message::SettingsProviderPicked,
    )
    .text_size(13)
    .padding([4, 8])
    // Match the full width of the text fields below it, so the form column
    // doesn't look ragged with a half-width dropdown.
    .width(Fill)
    .into();

    let key = text_input("paste your API key", &app.settings.key)
        .on_input(Message::SettingsKeyChanged)
        .secure(true)
        .size(13)
        .padding(6);
    let model = text_input(app.settings.provider.default_model(), &app.settings.model)
        .on_input(Message::SettingsModelChanged)
        .size(13)
        .padding(6);
    let base = text_input(
        app.settings.provider.default_base_url(),
        &app.settings.base_url,
    )
    .on_input(Message::SettingsBaseUrlChanged)
    .size(13)
    .padding(6);

    // Embeddings (semantic search) — an OpenAI-compatible endpoint.
    let embed_key = text_input("embedding API key", &app.settings.embed_key)
        .on_input(Message::SettingsEmbedKeyChanged)
        .secure(true)
        .size(13)
        .padding(6);
    let embed_model = text_input("text-embedding-3-small", &app.settings.embed_model)
        .on_input(Message::SettingsEmbedModelChanged)
        .size(13)
        .padding(6);
    let embed_base = text_input("https://api.openai.com/v1", &app.settings.embed_base_url)
        .on_input(Message::SettingsEmbedBaseUrlChanged)
        .size(13)
        .padding(6);

    let section = |s: &str| text(s.to_string()).size(12).color(theme::accent());

    // A small segmented control: System / Light / Dark, active one filled.
    let theme_btn = |p: crate::theme::ThemePref| -> Element<'_, Message> {
        let active = app.theme_pref == p;
        let style: fn(&iced::Theme, iced::widget::button::Status) -> iced::widget::button::Style =
            if active {
                theme::primary_button
            } else {
                theme::secondary_button
            };
        button(text(p.label()).size(12))
            .style(style)
            .padding([4, 16])
            .on_press(Message::SetThemePref(p))
            .into()
    };
    let appearance = iced::widget::row(crate::theme::ThemePref::ALL.map(theme_btn)).spacing(6);

    // Per-mode theme pickers: which theme fills the light slot, and the dark.
    use crate::theme::ThemeChoice;
    let light_pick: Element<'_, Message> = pick_list(
        crate::theme::light_choices(),
        Some(ThemeChoice(crate::theme::current_light())),
        |c: ThemeChoice| Message::SetThemeVariant {
            id: c.0.id,
            is_light: true,
        },
    )
    .text_size(13)
    .padding([4, 8])
    .width(Fill)
    .into();
    let dark_pick: Element<'_, Message> = pick_list(
        crate::theme::dark_choices(),
        Some(ThemeChoice(crate::theme::current_dark())),
        |c: ThemeChoice| Message::SetThemeVariant {
            id: c.0.id,
            is_light: false,
        },
    )
    .text_size(13)
    .padding([4, 8])
    .width(Fill)
    .into();

    let panel = container(
        column![
            row![
                text("Settings").size(16).color(theme::fg()),
                space().width(Fill),
                button(text("Save").size(12))
                    .style(theme::primary_button)
                    .padding([3, 14])
                    .on_press(Message::SettingsSaved),
                button(text("Close").size(12))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::CloseSettings),
            ]
            .spacing(6)
            .align_y(iced::Center),
            section("Appearance"),
            appearance,
            field("Light theme", light_pick),
            field("Dark theme", dark_pick),
            section("Updates"),
            iced::widget::checkbox(app.update.auto_check)
                .label("Check for updates automatically")
                .on_toggle(Message::SetAutoUpdate)
                .text_size(12)
                .size(16)
                .spacing(8),
            text(format!(
                "You have clew {}",
                crate::updater::current_version()
            ))
            .size(10)
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
                .size(10)
                .color(theme::dim()),
        ]
        .spacing(12),
    )
    .width(480)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseSettings))
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
        return space().into();
    };

    let title = row![
        glyph::icon(Glyph::Remote, theme::accent(), 18.0),
        text("Connect to Remote").size(16).color(theme::fg()),
        space().width(Fill),
        button(text("Close").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::CloseConnect),
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
                    .size(13)
                    .color(theme::fg()),
                text("Preparing the server on the remote host.")
                    .size(11)
                    .color(theme::dim()),
                space().height(6),
                button(text("Cancel").size(12))
                    .style(theme::toolbar_button)
                    .padding([4, 14])
                    .on_press(Message::CloseConnect),
            ]
            .spacing(6)
            .align_x(iced::Center),
        )
        .height(Length::Fixed(260.0))
        .into(),
        ConnectStage::Browsing(browser) => remote_browser_view(browser),
    };

    let panel = container(column![title, body].spacing(14))
        .width(560)
        .max_height(620)
        .padding(20)
        .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseConnect))
}

/// The picking stage: a list of saved hosts (if any) above a new-connection form.
pub(crate) fn connect_picker<'a>(
    app: &'a App,
    ui: &'a crate::ConnectUi,
    error: Option<&'a str>,
) -> Element<'a, Message> {
    use crate::ConnectField;
    let label = |s: &str| text(s.to_string()).size(11).color(theme::dim());

    let mut col = Column::new().spacing(12);

    if let Some(msg) = error {
        col = col.push(
            container(text(msg.to_string()).size(12).color(theme::danger()))
                .padding([6, 10])
                .width(Fill)
                .style(theme::modal_panel),
        );
    }

    // Saved hosts: click a row to connect, × to forget.
    if !app.saved_connections.is_empty() {
        col = col.push(section_header("SAVED HOSTS"));
        let mut list = Column::new().spacing(2);
        for (idx, conn) in app.saved_connections.iter().enumerate() {
            let open = button(
                row![
                    glyph::icon(Glyph::Remote, theme::fg_muted(), 14.0),
                    column![
                        text(conn.label()).size(13).color(theme::fg()),
                        text(conn.user_host()).size(11).color(theme::dim()),
                    ]
                    .spacing(1),
                ]
                .spacing(8)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 10])
            .on_press(Message::ConnectToSaved(idx));
            let remove = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
                .style(theme::toolbar_button)
                .padding([5, 8])
                .on_press(Message::ConnectRemoveSaved(idx));
            list = list.push(row![open, remove].spacing(4).align_y(iced::Center));
        }
        col = col.push(list);
    }

    // New-connection form.
    let field = |title: &str, input: Element<'a, Message>| column![label(title), input].spacing(3);
    let input = |placeholder: &str, value: &str, f: ConnectField| {
        text_input(placeholder, value)
            .on_input(move |s| Message::ConnectField(f, s))
            .size(13)
            .padding(6)
    };

    let identity = row![
        input(
            "(optional) ~/.ssh/id_ed25519",
            &ui.identity,
            ConnectField::Identity
        )
        .width(Fill),
        button(text("Browse…").size(12))
            .style(theme::toolbar_button)
            .padding([6, 12])
            .on_press(Message::ConnectPickIdentity),
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
    col = col.push(
        row![
            space().width(Fill),
            button(text("Connect").size(13))
                .style(theme::primary_button)
                .padding([6, 18])
                .on_press(Message::ConnectSubmit),
        ]
        .align_y(iced::Center),
    );

    // While connected to a remote, offer a way back to local reading.
    if app.connection.is_remote() {
        col = col.push(row![
            space().width(Fill),
            button(text("Disconnect (read local code)").size(11))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::ConnectDisconnect),
        ]);
    }

    scrollable(col.width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Shrink)
        .into()
}

/// The browsing stage: a path bar with an "up" control, the directory's contents
/// (folders navigable, files dimmed for context), and "Open this folder".
pub(crate) fn remote_browser_view(browser: &crate::RemoteBrowser) -> Element<'_, Message> {
    let mut up = button(glyph::icon(Glyph::ArrowLeft, theme::fg_muted(), 14.0))
        .style(theme::toolbar_button)
        .padding([4, 10]);
    if let Some(parent) = &browser.parent {
        up = up.on_press(Message::RemoteBrowseTo(parent.clone()));
    }
    let path_bar = row![
        up,
        container(
            text(browser.cwd.clone())
                .size(12)
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
            container(text(msg).size(12).color(theme::dim()))
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
                        text(entry.name.clone()).size(13).wrapping(Wrapping::None),
                    ]
                    .spacing(4)
                    .align_y(iced::Center),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([4, 8])
                .on_press(Message::RemoteBrowseTo(remote_join(
                    &browser.cwd,
                    &entry.name,
                )))
                .into(),
            );
        } else {
            let (glyph, color) = crate::icons::file_icon(&entry.name);
            rows.push(
                row![
                    tree_icon(glyph, color),
                    text(entry.name.clone())
                        .size(13)
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

    let entries = scrollable(Column::with_children(rows).spacing(1).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Length::Fixed(300.0));

    let footer = row![
        column![
            text("Open this folder as the project")
                .size(11)
                .color(theme::dim()),
            text(browser.cwd.clone())
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
        ]
        .spacing(1)
        .width(Fill),
        button(text("Open").size(13))
            .style(theme::primary_button)
            .padding([6, 18])
            .on_press(Message::RemoteOpenHere),
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
    let section = |s: &str| text(s.to_string()).size(12).color(theme::accent());

    // Header: title, optional "Reset all", Close.
    let mut header = row![
        text("Keyboard Shortcuts").size(16).color(theme::fg()),
        space().width(Fill)
    ]
    .spacing(6)
    .align_y(iced::Center);
    if app.keymap.any_overridden() {
        header = header.push(
            button(text("Reset all").size(12))
                .style(theme::toolbar_button)
                .padding([3, 12])
                .on_press(Message::RebindResetAll),
        );
    }
    header = header.push(
        button(text("Close").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::CloseShortcuts),
    );

    // A one-line hint, replaced by a warning when a rebind is rejected.
    let notice: Element<'_, Message> = match &app.keymap_notice {
        Some(msg) => text(msg.clone())
            .size(11)
            .color(theme::rgb(0xff9558))
            .into(),
        None => text("Click a shortcut, then press the new keys. Esc cancels.")
            .size(11)
            .color(theme::dim())
            .into(),
    };

    // Rebindable command rows.
    let mut cmds = Column::new().spacing(2);
    for action in Action::ALL {
        let binding: Element<'_, Message> = if app.rebinding == Some(action) {
            container(
                text("Press a shortcut… esc to cancel")
                    .size(12)
                    .color(theme::accent()),
            )
            .padding([3, 8])
            .into()
        } else {
            let pill = button(
                text(app.keymap.chord(action).caps())
                    .size(13)
                    .color(theme::fg()),
            )
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(Message::RebindStart(action));
            if app.keymap.is_overridden(action) {
                row![
                    pill,
                    button(text("↺").size(13).color(theme::dim()))
                        .style(theme::toolbar_button)
                        .padding([3, 7])
                        .on_press(Message::RebindReset(action)),
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
                text(action.label()).size(13).color(theme::fg()),
                space().width(Fill),
                binding,
            ]
            .align_y(iced::Center)
            .spacing(10)
            .padding([1, 2]),
        );
    }

    // Read-only reading motions (not part of the customizable keymap).
    let motions: [(&str, &str); 13] = [
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
        ("Clear selection / close", "esc"),
    ];
    let mut vim = Column::new().spacing(2);
    for (label, keys) in motions {
        vim = vim.push(
            row![
                text(label).size(13).color(theme::fg()),
                space().width(Fill),
                text(keys).size(12).color(theme::dim()),
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
    .height(Length::Fixed(440.0));

    let panel = container(
        column![
            header,
            notice,
            scroll_body,
            text(format!("Saved to {}", crate::llm::config_hint()))
                .size(10)
                .color(theme::dim()),
        ]
        .spacing(12),
    )
    .width(540)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::CloseShortcuts))
}
