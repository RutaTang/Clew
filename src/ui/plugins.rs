//! What plugins put on screen (see `app::plugins`): the page a command shows
//! over the code, the question before one is installed, the line of text a
//! command asks for, and Settings → Plugins.
//!
//! A plugin gives its page as data; it is drawn here with Clew's own
//! widgets, so the theme, the keyboard ring, jump badges and voice reach it
//! as they reach Clew's own pages. The words a plugin writes are shown as it
//! wrote them; the words around them are Clew's, translated.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use clew_plugin::runtime::{Block, Level, Location, View};

use crate::app::plugins::{Confirm, Installed, PendingInstall, PluginMsg, PluginPage, Prompt};
use clew_plugin::store::Origin;

/// The prompt's field, focused as it opens.
pub(crate) fn plugin_prompt_id() -> iced::widget::Id {
    iced::widget::Id::new("plugin-prompt")
}

/// The place row `row` of block `block` of `view` opens, if it opens one.
pub(crate) fn plugin_row_place(view: &View, block: usize, row: usize) -> Option<Location> {
    match view.blocks.get(block)? {
        Block::Items(items) => items.get(row)?.at.clone(),
        Block::Tree(rows) => rows.get(row)?.at.clone(),
        Block::Table(table) => table.rows.get(row)?.at.clone(),
        Block::Code(code) => code.at.clone(),
        _ => None,
    }
}

/// What the permissions `bits` let a plugin do, in one line of the
/// reader's words (Settings → Plugins).
pub(crate) fn permission_line(bits: u8) -> String {
    let read = bits & clew_rules::plugin::READ != 0;
    let git = bits & clew_rules::plugin::GIT != 0;
    match (read, git) {
        (true, true) => tr!("Reads the open project's files and history"),
        (true, false) => tr!("Reads the open project's files"),
        (false, true) => tr!("Reads the open project's history"),
        (false, false) => tr!("Reads none of your files"),
    }
    .to_string()
}

/// A notice's colour.
fn level_color(level: Level) -> iced::Color {
    match level {
        Level::Info => theme::info(),
        Level::Warning => theme::warning(),
        Level::Error => theme::danger(),
    }
}

/// `place` as the row shows it: `src/a.rs:12`, or the file alone.
fn place_words(at: &Location) -> String {
    if at.line > 0 {
        format!("{}:{}", at.path, at.line)
    } else {
        at.path.clone()
    }
}

/// A row that opens the code it points at, or just says its words.
fn page_row<'a>(
    title: String,
    detail: Option<String>,
    tag: Option<String>,
    at: Option<&Location>,
    color: iced::Color,
    indent: f32,
    open: Message,
) -> Element<'a, Message> {
    let mut words = column![
        text(title.clone())
            .size(ts::BASE)
            .color(color)
            .wrapping(Wrapping::WordOrGlyph)
    ]
    .spacing(2);
    if let Some(detail) = detail {
        words = words.push(text(detail).size(ts::SMALL).color(theme::dim()));
    }
    let mut line = row![space().width(indent), words.width(Fill)]
        .spacing(10)
        .align_y(iced::Center);
    if let Some(at) = at {
        line = line.push(
            text(place_words(at))
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None),
        );
    }
    if let Some(tag) = tag {
        line = line.push(
            container(
                text(tag)
                    .size(ts::CAPTION)
                    .color(theme::fg_muted())
                    .wrapping(Wrapping::None),
            )
            .padding([1, 6])
            .style(theme::chip),
        );
    }
    let body = button(line)
        .style(theme::list_row(false))
        .width(Fill)
        .padding([5, 8]);
    match at {
        Some(_) => body
            .on_press(super::heard::row(move || title.clone(), open))
            .into(),
        None => body.into(),
    }
}

/// The page a plugin's command showed.
pub(crate) fn plugin_page(page: &PluginPage) -> Element<'_, Message> {
    let close = Message::Plugin(PluginMsg::ClosePage);
    let header = row![
        column![
            text(page.view.title.clone())
                .size(ts::HEADING)
                .color(theme::fg()),
            text(page.plugin.clone())
                .size(ts::SMALL)
                .color(theme::dim()),
        ]
        .spacing(2)
        .width(Fill),
        super::chrome::close_button(close),
    ]
    .align_y(iced::Center);

    let mut blocks: Vec<Element<'_, Message>> = Vec::new();
    for (b, block) in page.view.blocks.iter().enumerate() {
        let open = |row: usize| Message::Plugin(PluginMsg::Open { block: b, row });
        let shown: Element<'_, Message> = match block {
            Block::Heading(words) => text(words.clone())
                .size(ts::EMPHASIS)
                .color(theme::fg())
                .into(),
            Block::Text(_) => match page.markdown.get(&b) {
                Some(items) => iced::widget::markdown::view(items, theme::markdown_settings())
                    .map(|url| Message::Plugin(PluginMsg::Link(url.to_string()))),
                None => space().height(0).into(),
            },
            Block::Notice(notice) => container(
                text(notice.text.clone())
                    .size(ts::BASE)
                    .color(level_color(notice.level)),
            )
            .padding([8, 12])
            .width(Fill)
            .style(theme::chip)
            .into(),
            Block::Items(items) => {
                Column::with_children(items.iter().enumerate().map(|(i, item)| {
                    page_row(
                        item.title.clone(),
                        item.detail.clone(),
                        item.tag.clone(),
                        item.at.as_ref(),
                        item.level.map_or(theme::fg(), level_color),
                        0.0,
                        open(i),
                    )
                }))
                .spacing(1)
                .into()
            }
            Block::Tree(rows) => Column::with_children(rows.iter().enumerate().map(|(i, r)| {
                page_row(
                    r.title.clone(),
                    r.detail.clone(),
                    r.tag.clone(),
                    r.at.as_ref(),
                    theme::fg(),
                    r.depth as f32 * 18.0,
                    open(i),
                )
            }))
            .spacing(1)
            .into(),
            Block::Table(table) => {
                let head = Row::with_children(table.columns.iter().map(|c| {
                    text(c.clone())
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .width(Fill)
                        .into()
                }))
                .spacing(12)
                .padding([0, 8]);
                let rows = table
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(i, r)| -> Element<'_, Message> {
                        let cells = Row::with_children(r.cells.iter().map(|c| {
                            text(c.clone())
                                .size(ts::BASE)
                                .color(theme::fg())
                                .width(Fill)
                                .into()
                        }))
                        .spacing(12);
                        let body = button(cells)
                            .style(theme::list_row(false))
                            .width(Fill)
                            .padding([5, 8]);
                        match &r.at {
                            Some(_) => {
                                let name = r.cells.first().cloned().unwrap_or_default();
                                body.on_press(super::heard::row(move || name.clone(), open(i)))
                                    .into()
                            }
                            None => body.into(),
                        }
                    });
                column![head].extend(rows).spacing(1).into()
            }
            Block::Facts(facts) => Column::with_children(facts.iter().map(|f| {
                row![
                    text(f.name.clone())
                        .size(ts::BASE)
                        .color(theme::dim())
                        .width(Length::Fixed(220.0)),
                    text(f.value.clone()).size(ts::BASE).color(theme::fg()),
                ]
                .spacing(12)
                .into()
            }))
            .spacing(4)
            .into(),
            Block::Code(code) => {
                let lines = page
                    .code
                    .get(&b)
                    .map(|lines| super::explain_view::code_block(lines))
                    .unwrap_or_else(|| space().height(0).into());
                let framed = container(scrollable(lines).direction(thin_scroll()))
                    .padding(8)
                    .width(Fill)
                    .style(theme::editor);
                match &code.at {
                    Some(at) => column![
                        button(text(place_words(at)).size(ts::SMALL).color(theme::accent()))
                            .style(theme::toolbar_button)
                            .padding([2, 6])
                            .on_press(super::heard::said(&place_words(at), open(0))),
                        framed
                    ]
                    .spacing(4)
                    .into(),
                    None => framed.into(),
                }
            }
        };
        blocks.push(shown);
    }
    if page.view.blocks.is_empty() {
        blocks.push(
            text(tr!("The plugin had nothing to show."))
                .size(ts::BASE)
                .color(theme::dim())
                .into(),
        );
    }
    container(
        column![
            header,
            scrollable(
                Column::with_children(blocks)
                    .spacing(14)
                    .width(Fill)
                    .max_width(960)
            )
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
        ]
        .spacing(14),
    )
    .width(Fill)
    .height(Fill)
    .padding([20, 28])
    .into()
}

/// Where a plugin waiting to be installed came from, as the card says it.
fn origin_words(origin: &Origin) -> String {
    match origin {
        Origin::Ask => tr!("Made by the agent in Ask").to_string(),
        Origin::Agent(name) => tr!("Made by {agent}", agent = name.clone()),
        Origin::File(path) => tr!("From the file {path}", path = path.clone()),
    }
}

/// Ask whether to install a plugin: what it is, what it adds, what it may
/// do — new permissions marked — and where it came from.
pub(crate) fn plugin_install_modal(
    pending: &PendingInstall,
    number: u64,
    waiting: usize,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'_, Message> {
    let asked = asked.into();
    let ready = asked.ready;
    let manifest = &pending.inspected.manifest;
    let lang = crate::i18n::current().code();
    let name = manifest.name_in(lang).to_string();
    let title = match &pending.previous {
        Some(previous) if previous.manifest.plugin.version != manifest.plugin.version => tr!(
            "Update “{plugin}” to version {version}?",
            plugin = name.clone(),
            version = manifest.plugin.version.clone()
        ),
        Some(_) => tr!("Update “{plugin}”?", plugin = name.clone()),
        None => tr!("Install “{plugin}”?", plugin = name.clone()),
    };
    let mut adds: Vec<String> = manifest
        .commands
        .iter()
        .map(|c| {
            tr!(
                "A command: {title}",
                title = crate::app::plugins::command_title(c)
            )
        })
        .collect();
    adds.extend(manifest.graphs.iter().map(|g| {
        tr!(
            "A project graph: {title}",
            title = g.title_in(lang).to_string()
        )
    }));
    let new = pending.new_permissions();
    let mut can: Vec<Element<'_, Message>> = Vec::new();
    for (bit, words) in [
        (
            clew_rules::plugin::READ,
            tr!("Read the open project's files"),
        ),
        (
            clew_rules::plugin::GIT,
            tr!("Read the open project's history"),
        ),
    ] {
        if pending.inspected.needs & bit == 0 {
            continue;
        }
        let mut line = row![text(format!("• {words}")).size(ts::BASE).color(theme::fg())]
            .spacing(8)
            .align_y(iced::Center);
        if pending.previous.is_some() && new & bit != 0 {
            line = line.push(
                container(text(tr!("New")).size(ts::CAPTION).color(theme::warning()))
                    .padding([1, 6])
                    .style(theme::chip),
            );
        }
        can.push(line.into());
    }
    if can.is_empty() {
        can.push(
            text(format!("• {}", tr!("Nothing beyond showing its results")))
                .size(ts::BASE)
                .color(theme::fg())
                .into(),
        );
    }
    let section = |heading: String| text(heading).size(ts::SMALL).color(theme::dim());
    let (decline, accept) = (tr!("Don't install"), tr!("Install"));
    let panel = container(
        column![
            text(title).size(ts::TITLE).color(theme::fg()),
            text(manifest.description_in(lang).to_string())
                .size(ts::BASE)
                .color(theme::fg()),
            column![section(tr!("What it adds").to_string())]
                .extend(adds.into_iter().map(|a| {
                    text(format!("• {a}"))
                        .size(ts::BASE)
                        .color(theme::fg())
                        .into()
                }))
                .spacing(4),
            column![section(tr!("What it may do").to_string())]
                .extend(can)
                .spacing(4),
            text(tr!(
                "It can never reach the network, other files, programs or your keys."
            ))
            .size(ts::BODY)
            .color(theme::dim()),
            text(tr!(
                "{origin}, version {version}",
                origin = origin_words(&pending.origin),
                version = manifest.plugin.version.clone()
            ))
            .size(ts::SMALL)
            .color(theme::dim()),
        ]
        .push(super::explain_view::more_questions(waiting))
        .push(
            row![
                space().width(Fill),
                button(text(decline).size(ts::BASE))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(heard::answer(
                        decline,
                        heard::Answer::No,
                        Message::Plugin(PluginMsg::InstallDismissed(number))
                    )),
                crate::ui::fresh::allow(
                    ready,
                    button(text(accept).size(ts::BASE))
                        .style(theme::toolbar_button)
                        .padding([6, 16])
                        .on_press_maybe(heard::answer_when(
                            ready,
                            accept,
                            heard::Answer::Yes,
                            Message::Plugin(PluginMsg::InstallAllowed(number))
                        ))
                ),
            ]
            .spacing(10)
            .align_y(iced::Center),
        )
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);
    // The choice is required: the backdrop ignores clicks (Escape declines).
    let panel = crate::ui::placed::placed(&asked, crate::ui::fresh::fresh(number, panel));
    modal(panel, Placement::Center, Backdrop::Dim(None))
}

/// Something asked before it is done: a web address a plugin's page links
/// to, shown whole; a Remove.
pub(crate) fn plugin_confirm_modal(confirm: &Confirm) -> Element<'_, Message> {
    let no = tr!("Cancel");
    let yes = confirm.yes.as_str();
    let detail = text(confirm.detail.clone()).size(ts::BODY);
    let detail: Element<'_, Message> =
        if matches!(confirm.then, crate::app::plugins::Then::OpenLink(_)) {
            // A long address breaks anywhere, so all of it is seen.
            container(
                detail
                    .font(crate::fonts::CODE)
                    .color(theme::fg())
                    .wrapping(Wrapping::WordOrGlyph),
            )
            .padding([8, 10])
            .width(Fill)
            .style(theme::trough)
            .into()
        } else {
            detail.color(theme::dim()).into()
        };
    let panel = container(
        column![
            text(confirm.title.clone())
                .size(ts::EMPHASIS)
                .color(theme::fg()),
            detail,
            row![
                space().width(Fill),
                button(text(no).size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(heard::answer(
                        no,
                        heard::Answer::No,
                        Message::Plugin(PluginMsg::ConfirmDismissed)
                    )),
                button(
                    text(yes)
                        .size(ts::BASE)
                        .color_maybe(confirm.destructive.then(theme::danger))
                )
                .style(if confirm.destructive {
                    theme::toolbar_button
                } else {
                    theme::primary_button
                })
                .padding([6, 16])
                .on_press(heard::answer(
                    yes,
                    heard::Answer::Yes,
                    Message::Plugin(PluginMsg::Confirmed)
                )),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);
    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Plugin(PluginMsg::ConfirmDismissed))),
    )
}

/// The line of text a command asks for before it runs.
pub(crate) fn plugin_prompt_modal(prompt: &Prompt) -> Element<'_, Message> {
    super::chrome::prompt_modal(super::chrome::Prompt {
        title: prompt.title.clone(),
        question: Some(&prompt.asks),
        placeholder: "",
        value: &prompt.text,
        input_id: plugin_prompt_id(),
        on_input: |v| Message::Plugin(PluginMsg::PromptEdit(v)),
        submit: Message::Plugin(PluginMsg::PromptRun),
        submit_label: tr!("Run"),
        cancel: Message::Plugin(PluginMsg::PromptCancel),
        remove: None,
    })
}

/// One installed plugin, in Settings: its name and what it is, what it may
/// do, a switch, and Remove; its log's last lines under it.
fn installed_row<'a>(app: &'a App, plugin: &'a Installed) -> Element<'a, Message> {
    let can = permission_line(plugin.record.granted());
    let lang = crate::i18n::current().code();
    let commands = plugin
        .manifest
        .commands
        .iter()
        .map(crate::app::plugins::command_title)
        .collect::<Vec<_>>()
        .join(", ");
    let graphs = plugin
        .manifest
        .graphs
        .iter()
        .map(|g| g.title_in(lang).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    // What it adds, where each is: the palette's commands, Project
    // Graphs' graphs. A plugin of graphs only said "Commands:" and nothing.
    let adds = [
        (!commands.is_empty()).then(|| tr!("Commands: {list}", list = commands)),
        (!graphs.is_empty()).then(|| tr!("Graphs in Project Graphs: {list}", list = graphs)),
    ];
    let id = plugin.id.clone();
    let mut item = column![
        row![
            column![
                text(format!(
                    "{} {}",
                    plugin.name(),
                    plugin.manifest.plugin.version
                ))
                .size(ts::BASE)
                .color(theme::fg()),
                text(plugin.description())
                    .size(ts::BODY)
                    .color(theme::fg_muted()),
            ]
            .extend(
                adds.into_iter()
                    .flatten()
                    .map(|line| text(line).size(ts::SMALL).color(theme::dim()).into()),
            )
            .push(text(can).size(ts::SMALL).color(theme::dim()))
            .spacing(3)
            .width(Fill),
            form::switch(
                plugin.record.enabled,
                Message::Plugin(PluginMsg::SetEnabled(id.clone(), !plugin.record.enabled))
            ),
            form::action(
                tr!("Remove"),
                Some(Message::Plugin(PluginMsg::AskRemove(id.clone())))
            ),
        ]
        .spacing(12)
        .align_y(iced::Center),
    ]
    .spacing(6);
    if let Some(log) = app.plugins.log.get(&plugin.id).filter(|l| !l.is_empty()) {
        let last: Vec<Element<'_, Message>> = log
            .iter()
            .rev()
            .take(5)
            .rev()
            .map(|(level, line)| {
                text(line.clone())
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(match level {
                        Level::Info => theme::dim(),
                        other => level_color(*other),
                    })
                    .into()
            })
            .collect();
        item = item.push(
            container(Column::with_children(last).spacing(2))
                .padding([6, 10])
                .width(Fill)
                .style(theme::editor),
        );
    }
    form::row_of(item)
}

/// Why plugins cannot be built here, in the reader's words.
fn missing_words(why: &clew_plugin::toolchain::Why) -> String {
    use clew_plugin::toolchain::Why;
    match why {
        Why::NoRust => tr!("Rust is not installed on this computer.").into(),
        Why::NoDefault(_) => tr!("rustup is installed, but has no default Rust.").into(),
        Why::NoTarget(version) => tr!(
            "Rust {version} is installed, but not the WebAssembly target plugins are built for.",
            version = version.clone()
        ),
        Why::TooOld(version) => tr!(
            "Rust {version} is installed, but it is too old to build plugins.",
            version = version.clone()
        ),
        Why::OwnBroken(_) => tr!("The Rust Clew installed does not work.").into(),
        Why::Other(why) => tr!(
            "Rust cannot build plugins here: {reason}",
            reason = why.clone()
        ),
    }
}

/// Settings → Plugins: whether plugins can be built here, and the one
/// step that makes it so.
fn building_group(app: &App) -> Element<'_, Message> {
    use crate::app::plugins::Building;
    use clew_plugin::toolchain::{Setup, Stage};
    let none = || -> Element<'_, Message> { space().width(0).into() };
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    match &app.plugins.building {
        Building::Unknown | Building::Checking => {
            rows.push(form::item(
                tr!("Rust"),
                Some(tr!("Checking…").into()),
                none(),
            ));
        }
        Building::Ready { version, own } => {
            let said = match own {
                true => tr!(
                    "Ready: Rust {version}, installed by Clew",
                    version = version.clone()
                ),
                false => tr!("Ready: Rust {version}", version = version.clone()),
            };
            rows.push(form::item(tr!("Rust"), Some(said), none()));
        }
        Building::Missing(missing) => {
            let (action, about) = match &missing.setup {
                Some(Setup::AddTarget { .. }) => (
                    Some(tr!("Add the WebAssembly target")),
                    tr!(
                        "Runs “rustup target add wasm32-wasip2”, which adds about 100 MB to your Rust."
                    ),
                ),
                Some(Setup::Install) => (
                    Some(tr!("Install Rust for Clew")),
                    tr!(
                        "Installs Rust from rust-lang.org into Clew's own folder (about 700 MB). Nothing else on this computer changes."
                    ),
                ),
                None => (
                    None,
                    tr!("Install Rust from rustup.rs, then open this page again."),
                ),
            };
            let control: Element<'_, Message> = match action {
                Some(action) => {
                    form::primary(action, Some(Message::Plugin(PluginMsg::SetUpToolchain))).into()
                }
                None => none(),
            };
            rows.push(form::item_saying(
                tr!("Rust"),
                Some(missing_words(&missing.why)),
                true,
                control,
            ));
            rows.push(form::row_of(form::note(about)));
        }
        Building::SettingUp { stage, .. } => {
            let said = match stage {
                Stage::Downloading => tr!("Downloading Rust's installer…"),
                Stage::Installing => {
                    tr!("Installing Rust… This takes a few minutes.")
                }
                Stage::AddingTarget => tr!("Adding the WebAssembly target…"),
            };
            // Adding the target runs to its end (`CancelToolchainSetup`).
            let cancel: Element<'_, Message> = if *stage == Stage::AddingTarget {
                none()
            } else {
                form::action(
                    tr!("Cancel"),
                    Some(Message::Plugin(PluginMsg::CancelToolchainSetup)),
                )
                .into()
            };
            rows.push(form::item(tr!("Rust"), Some(said.into()), cancel));
        }
    }
    if let Some(why) = &app.plugins.setup_failure {
        rows.push(form::row_of(
            form::note(tr!(
                "Setting up did not work: {reason}",
                reason = why.clone()
            ))
            .color(theme::danger()),
        ));
    }
    // Clew's own Rust, removable while nothing is being set up.
    if app.plugins.own_rust
        && !matches!(
            app.plugins.building,
            Building::SettingUp { .. } | Building::Checking
        )
    {
        let said = match &app.plugins.building {
            Building::Ready { own: true, .. } => tr!(
                "Removing it frees about 700 MB. Plugins already installed keep working; building new ones needs Rust again."
            ),
            Building::Ready { own: false, .. } => {
                tr!("Not in use: your own Rust is ready. Removing it frees about 700 MB.")
            }
            _ => tr!("It does not work as it is. Removing it frees about 700 MB."),
        };
        rows.push(form::item(
            tr!("Clew's own Rust"),
            Some(said.into()),
            form::action(
                tr!("Remove"),
                Some(Message::Plugin(PluginMsg::AskRemoveOwnRust)),
            ),
        ));
    }
    form::group(
        tr!("Building plugins"),
        Some(tr!("Agents write plugins in Rust, and Clew builds them on this computer.").into()),
        None,
        rows,
    )
}

/// Settings → Plugins.
pub(crate) fn plugins_settings(app: &App) -> Element<'_, Message> {
    let install = form::action(
        tr!("Install from a file…"),
        Some(Message::Plugin(PluginMsg::InstallFromFile)),
    );
    let mut groups: Vec<Element<'_, Message>> = Vec::new();
    if app.plugins.installed.is_empty() {
        groups.push(form::group(
            "",
            Some(
                tr!(
                    "A plugin adds what you need to Clew. Ask your agent for one, in your own words: “make a plugin that lists the TODOs by who wrote them”. It writes the plugin, and Clew asks you before installing it."
                )
                .to_string(),
            ),
            None,
            vec![form::item(tr!("Have a plugin's file?"), None, install)],
        ));
    } else {
        groups.push(form::group(
            "",
            None,
            None,
            app.plugins
                .installed
                .iter()
                .map(|p| installed_row(app, p))
                .chain(std::iter::once(form::item(
                    tr!("Have a plugin's file?"),
                    None,
                    install,
                )))
                .collect(),
        ));
    }
    if let Some(why) = &app.plugins.failure {
        groups.push(form::note(why.clone()).color(theme::danger()).into());
    }
    groups.push(building_group(app));
    groups.push(
        form::note(tr!(
            "Plugins run inside Clew, apart from everything else: each can do only what you allowed when you installed it, and never reach the network, other files, programs or your keys."
        ))
        .into(),
    );
    form::page(groups)
}
