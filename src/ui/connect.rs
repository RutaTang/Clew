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
        ConnectStage::TrustHost { target, key, .. } => trust_host_prompt(target, key),
        ConnectStage::HostKeyChanged {
            target,
            reason,
            forget_host,
        } => host_key_changed_prompt(target, reason, forget_host),
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
) -> Element<'a, Message> {
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
    column![
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
            button(text("Trust and connect").size(ts::BODY))
                .style(theme::primary_button)
                .padding([6, 14])
                .on_press(Message::Connect(ConnectMsg::TrustHost {
                    fingerprint: key.fingerprint.clone(),
                })),
        ]
        .spacing(8)
        .align_y(iced::Center),
    ]
    .spacing(10)
    .into()
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
) -> Element<'a, Message> {
    let para = |s: String| {
        text(s)
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
    };
    column![
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
            button(text("Forget the old key").size(ts::BODY))
                .style(theme::toolbar_button)
                .padding([6, 14])
                .on_press(Message::Connect(ConnectMsg::ForgetHostKey {
                    host: forget_host.to_string(),
                })),
        ]
        .spacing(8)
        .align_y(iced::Center),
    ]
    .spacing(10)
    .into()
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

    let mut col = Column::new().spacing(12);

    if let Some(msg) = error {
        col = col.push(
            container(text(msg.to_string()).size(ts::BODY).color(theme::danger()))
                .padding([6, 10])
                .width(Fill)
                .style(theme::modal_panel),
        );
    }

    // Saved hosts: click a row to connect, × to forget.
    if !app.saved_connections.is_empty() {
        col = col.push(section_header("SAVED HOSTS"));
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
        col = col.push(list);
    }

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

    scrollable(col.width(Fill))
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
