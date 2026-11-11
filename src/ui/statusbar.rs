//! Status bar and refresh chip.

use super::*;

pub(crate) fn short_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "fn",
        "method" => "meth",
        "class" => "class",
        "struct" => "struct",
        "enum" => "enum",
        "union" => "union",
        "trait" => "trait",
        "interface" => "iface",
        "implementation" => "impl",
        "module" => "mod",
        "macro" => "macro",
        "constant" => "const",
        "type" => "type",
        "table" => "table",
        "view" => "view",
        "trigger" => "trig",
        "theorem" => "thm",
        "instance" => "inst",
        "inductive" => "ind",
        "axiom" => "axiom",
        // Notebook outline entries: code cells and markdown headings.
        "cell" => "cell",
        "section" => "§",
        _ => "sym",
    }
}

// ---------------------------------------------------------------- status bar

/// Height of the status bar. Fixed, so the tutorial can carve the body region
/// out of the window and the target drop-up can sit right above it.
pub(crate) const STATUSBAR_H: f32 = 32.0;

/// Between the status bar's items.
const STATUS_GAP: f32 = 18.0;
/// Height of the target picker at the bar's end.
const PICKER_H: f32 = 22.0;
/// The room a language server's own words of what it is doing have, while
/// it is at work.
const LSP_BUSY_W: f32 = 240.0;
/// The most room what the language server's state says may take.
const LSP_MAX_W: f32 = 340.0;

/// One quiet item of the status bar.
fn status_text<'a>(s: impl iced::widget::text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(ts::SMALL).color(theme::fg_muted())
}

/// Whether the project is current: scanned and indexed, or what it is
/// doing to be. `None` with no project open.
fn sync_state(app: &App) -> Option<&'static str> {
    app.proj.project.as_ref()?;
    Some(if app.scanning {
        tr!("Scanning…")
    } else if app.proj.indexing {
        tr!("Indexing…")
    } else {
        tr!("Up to date")
    })
}

/// `n` with its thousands set apart, as the reader writes it: 1,858.
pub(crate) fn thousands(n: usize) -> String {
    crate::i18n::number(n as u64)
}

/// The platform a file's branches are read for, as the reader says it: its
/// system's name ("Windows"), and its architecture only where that is not
/// the system's usual one ("Windows (arm64)"). The machine's own is its
/// system's name. A target of no known system is called by its label.
pub(crate) fn target_name(target: &crate::inactive::Target) -> String {
    let system = match target.os.as_str() {
        "linux" => "Linux",
        "macos" => "macOS",
        "windows" => "Windows",
        "freebsd" => "FreeBSD",
        "ios" => "iOS",
        "android" => "Android",
        _ if target.family == "wasm" => "Web",
        _ => return target.label.clone(),
    };
    // The architecture each is usually read for: said only when it is
    // another.
    let usual = match target.os.as_str() {
        "macos" | "ios" | "android" => "aarch64",
        _ if target.family == "wasm" => "wasm32",
        _ => "x86_64",
    };
    if target.label.starts_with("Host") || target.arch == usual || target.arch.is_empty() {
        return system.to_string();
    }
    let arch = match target.arch.as_str() {
        "aarch64" => "arm64",
        arch => arch,
    };
    format!("{system} ({arch})")
}

/// A reading target as its picker lists it: its label, the machine's own
/// ("Host (linux)", clew-core's) in the reader's language.
pub(crate) fn target_label(target: &crate::inactive::Target) -> String {
    match target
        .label
        .strip_prefix("Host (")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(os) => tr!("Host ({os})", os = os),
        None => target.label.clone(),
    }
}

/// The longest branch name the status bar shows whole; a longer one is cut
/// to it, and said whole where the pointer rests.
const BRANCH_ROOM: usize = 28;

/// What the status bar says HEAD is: the branch's name, cut to
/// [`BRANCH_ROOM`] characters — or, detached, the commit's short name.
pub(crate) fn head_shown(head: &clew_protocol::HeadName) -> String {
    match head {
        clew_protocol::HeadName::Branch(name) if name.chars().count() > BRANCH_ROOM => {
            let cut: String = name.chars().take(BRANCH_ROOM - 1).collect();
            format!("{cut}…")
        }
        clew_protocol::HeadName::Branch(name) => name.clone(),
        clew_protocol::HeadName::Detached(sha) => super::panes::short_sha(sha),
    }
}

/// The branch checked out, after the place the code is read from — or the
/// commit a detached HEAD is at — with the whole of it where the pointer
/// rests. Nothing outside a repository.
fn branch_item(app: &App) -> Option<Element<'_, Message>> {
    let head = app.proj.head.as_ref()?;
    let about = match head {
        clew_protocol::HeadName::Branch(name) => {
            tr!("On branch {branch}", branch = name.as_str())
        }
        clew_protocol::HeadName::Detached(sha) => tr!(
            "Not on a branch: at commit {commit}",
            commit = super::panes::short_sha(sha)
        ),
    };
    // Set apart from the scan's state before it: "Up to date main" read
    // as the branch's sync with its remote. Outside the button: its hover
    // took the rule in, and stood wider before the branch than after it.
    let rule = container(space().width(1).height(12)).style(|_: &iced::Theme| container::Style {
        background: Some(theme::border().into()),
        ..container::Style::default()
    });
    let shown = row![
        glyph::icon(Glyph::Branch, theme::dim(), 12.0),
        status_text(head_shown(head)),
    ]
    .spacing(5)
    .align_y(iced::Center);
    // Pressed, the branch's history: its commits, in the History view —
    // which its tooltip says, as it named only the branch.
    let shown = button(shown)
        .style(theme::icon_button(false))
        .padding([2, 4])
        .on_press(super::heard::said(
            tr!("History"),
            Message::History(crate::HistoryMsg::Show { path: None }),
        ));
    let tip = iced::widget::column![
        text(about).size(ts::SMALL).color(theme::tip_text()),
        text(tr!("Click for its history"))
            .size(ts::SMALL)
            .color(theme::tip_text()),
    ]
    .spacing(2);
    Some(
        row![
            rule,
            super::tip::tip(
                shown,
                container(tip).padding([3, 7]).style(theme::tip),
                tooltip::Position::Top,
            )
            .gap(5.0),
        ]
        .spacing(4)
        .align_y(iced::Center)
        .into(),
    )
}

/// The status bar: one quiet line along the window's foot. On the left,
/// where the code is read from and what Clew is doing; on the right, the
/// open file — its language and language server, its problems, the caret,
/// and which platform its `#[cfg]`-style branches are read for.
pub(crate) fn statusbar(app: &App) -> Element<'_, Message> {
    // Where code is read from — the one place local vs remote shows. Click
    // it to manage connections / switch hosts.
    let remote = app.connection.is_remote();
    // Where the code is read from: this machine, or the host by its name.
    let place = if remote {
        app.connection.label()
    } else {
        tr!("Local").to_string()
    };
    let dot =
        container(space().width(6).height(6)).style(move |_: &iced::Theme| container::Style {
            background: Some(
                if remote {
                    theme::accent()
                } else {
                    theme::success()
                }
                .into(),
            ),
            border: iced::border::rounded(3),
            ..container::Style::default()
        });
    // Where the code is read from, alone in the button that opens Connect:
    // the scan's state beside it read as the place's — "Local · Up to
    // date ⌄" as a branch's sync with its remote.
    let place_text = text(place.clone()).size(ts::SMALL).color(if remote {
        theme::accent()
    } else {
        theme::fg_muted()
    });
    // Scanning, indexing, up to date: after the button, in a slot as wide
    // as the widest of them, so what Clew says beside it stays where it is.
    let sync: Element<'_, Message> = match sync_state(app) {
        Some(state) => {
            let slot = super::steady::slot(
                status_text(state),
                state,
                &[tr!("Scanning…"), tr!("Indexing…"), tr!("Up to date")],
                ts::SMALL,
            );
            // What is up to date: the Updates page's "Clew is up to date"
            // is of the app, this of the project's files.
            if state == tr!("Up to date") {
                super::tip::tip(
                    slot,
                    container(
                        text(tr!("Every file is scanned and indexed"))
                            .size(ts::SMALL)
                            .color(theme::tip_text()),
                    )
                    .padding([3, 7])
                    .style(theme::tip),
                    tooltip::Position::Top,
                )
                .gap(5.0)
                .into()
            } else {
                slot
            }
        }
        None => space().width(0).into(),
    };
    // A chevron after it, as the target picker has: it opens something —
    // Connect — which nothing said until the pointer rested on it (L23).
    let opens = glyph::icon(Glyph::ChevronDown, theme::dim(), 11.0);
    let conn_indicator = super::tip::tip(
        button(
            row![dot, place_text, opens]
                .spacing(7)
                .align_y(iced::Center),
        )
        .style(theme::icon_button(false))
        .padding([2, 8])
        .on_press(super::heard::said(
            tr!("Connect to a remote host"),
            Message::Connect(ConnectMsg::Open),
        )),
        // The design's tooltip bubble.
        container(
            text(tr!("Connect to a remote host"))
                .size(ts::SMALL)
                .color(theme::tip_text()),
        )
        .padding([3, 7])
        .style(theme::tip),
        tooltip::Position::Top,
    )
    .gap(5.0);

    // What stays while it lasts — setting up, a pass's progress — and what
    // Clew says, laid out for the room there is (`status_left`). Built only
    // when laid out, which a gathering of what voice can press never does:
    // built once more for it, here, so its chips are said as clicked.
    if super::heard::gathering() {
        drop(status_left(app, f32::INFINITY));
    }
    let said_row = container(iced::widget::responsive(move |room| {
        status_left(app, room.width)
    }))
    .width(Fill)
    .height(Length::Fixed(STATUS_LINE_H));
    // The place, its scan's state, then the branch: "Local · Up to date ·
    // ⎇ main", the state still read as the place's.
    let mut left = row![conn_indicator, sync];
    if let Some(branch) = branch_item(app) {
        left = left.push(branch);
    }
    let mut bar = row![
        left.push(said_row)
            .spacing(STATUS_GAP - 8.0)
            .align_y(iced::Center)
            .width(Fill)
    ]
    .align_y(iced::Center);
    if let Some(chip) = refresh_chip(app) {
        bar = bar.push(chip);
    }

    // In time travel, report the revision being viewed — not the live
    // document's (its line count / a caret line that may not exist in this
    // revision). Only while the session is on screen: off it, the document
    // that is.
    if let Some(tt) = time_travel_on_screen(app) {
        // Only what neither the banner (the commit) nor the scrubber (its
        // place, the scope) says: the revision's length. All of it was said
        // here a second time.
        let lines = tt
            .viewer
            .as_ref()
            .map(|v| trn!("1 line", "{n} lines", v.lines.len()))
            .unwrap_or_default();
        let widest = super::steady::widest_figure(&format!("{lines}0"));
        bar = bar.push(super::steady::slot(
            status_text(lines.clone()),
            &lines,
            &[&widest],
            ts::SMALL,
        ));
    } else if let Some(v) = app.active_viewer() {
        let lang = v
            .lang_key
            .and_then(crate::highlight::lang_name)
            .unwrap_or_else(|| untagged_kind(&v.abs));
        // Language-server status for this file's language, when relevant —
        // from the snapshot this update took (so a state clew cannot read
        // shows as a problem, not as "ready").
        // A server at work is named ("rust-analyzer ready"); one that is
        // not says why in words of its own.
        let server = v.lang_key.and_then(|k| {
            let status = app.lsp_status(k)?;
            let at_work = matches!(
                app.proj.link.lsp.get(k),
                Some(crate::LspSlot::Ready(_) | crate::LspSlot::Starting)
            );
            Some((
                crate::lsp::registry::default_for_language(k)
                    .filter(|_| at_work)
                    .map(|server| server.name),
                status,
            ))
        });
        let named = |status: &str| match server {
            Some((Some(name), _)) => format!("{lang}: {name} {status}"),
            Some((None, _)) => format!("{lang}: {status}"),
            None => lang.to_string(),
        };
        let said = match &server {
            Some((_, status)) => named(status),
            None => lang.to_string(),
        };
        // A server at work says what it is doing — "Indexing 12/340",
        // "Building proc-macros: serde" — many times a second, in words of
        // its own length. Meanwhile it has a slot of its own, cut to it,
        // that what stands beside it does not move by; at rest, a slot as
        // wide as the widest of starting and ready.
        let at_rest = [named(tr!("starting…")), named(tr!("ready"))];
        let busy = v.lang_key.is_some_and(|k| app.lsp_busy(k));
        // What it says, and the whole of it when that is cut short.
        let (lsp_text, whole): (Element<'_, Message>, Option<String>) = if busy {
            let width =
                super::steady::widest(&[&at_rest[0], &at_rest[1]], ts::SMALL, crate::fonts::UI)
                    .max(LSP_BUSY_W);
            let said = super::steady::cut(&said, width, ts::SMALL, crate::fonts::UI);
            (
                container(status_text(said).wrapping(iced::widget::text::Wrapping::None))
                    .width(width)
                    .clip(true)
                    .into(),
                None,
            )
        } else {
            // A server that could not be had says why at length ("'cargo' is
            // required to install this server but was not found on PATH;
            // …"): cut short to its room, said whole where the pointer
            // rests. At full length it took the bar's middle, and what Clew
            // said beside it was squeezed to nothing.
            let shown = super::steady::cut(&said, LSP_MAX_W, ts::SMALL, crate::fonts::UI);
            let slot = super::steady::slot(
                status_text(shown.clone()),
                &shown,
                &[&at_rest[0], &at_rest[1]],
                ts::SMALL,
            );
            (slot, (shown != said).then_some(said))
        };
        // A server's state opens the page that lists them, where what it
        // says to do ("…, or choose where it is") is done — as the place
        // opens Connect. Padded alike whether or not there is one, so the
        // words stand where they stood when a file of another language is
        // shown.
        let mut lsp_button = button(lsp_text)
            .style(theme::icon_button(false))
            .padding([2, 8]);
        if server.is_some() {
            lsp_button = lsp_button.on_press(super::heard::said(
                tr!("Language servers"),
                Message::Settings(SettingsMsg::OpenAt(crate::SettingsPage::Languages)),
            ));
        }
        let open = tr!("Open Settings → Language servers");
        let tip = match (server.is_some(), whole) {
            (true, Some(whole)) => Some(format!("{whole}\n{open}")),
            (true, None) => Some(open.to_string()),
            (false, whole) => whole,
        };
        bar = bar.push(match tip {
            Some(tip) => super::toolbar::tip_at(lsp_button, tip, None, tooltip::Position::Top),
            None => lsp_button.into(),
        });
        // Diagnostic counts for this file, from the same snapshot.
        let diags = v
            .lang_key
            .and_then(|k| match app.proj.link.lsp_snapshots.get(k) {
                Some(Ok(snapshot)) => Some(snapshot.diagnostics(&v.abs)),
                _ => None,
            })
            .map(|ds| {
                let errs = ds.iter().filter(|d| d.severity == 1).count();
                let warns = ds.iter().filter(|d| d.severity == 2).count();
                (errs, warns)
            });
        // Said whenever the server can say them, none included, each in a
        // slot two figures wide at least: a count that came and went with
        // the problems of the file moved everything beside it.
        if let Some((errs, warns)) = diags {
            let count = |mark: &str, n: usize, color: iced::Color| {
                let said = format!("{mark} {n}");
                let widest = super::steady::widest_figure(&format!("{mark} {n:02}"));
                super::steady::slot(
                    text(said.clone()).size(ts::SMALL).color(color),
                    &said,
                    &[&widest],
                    ts::SMALL,
                )
            };
            bar = bar.push(
                row![
                    count(
                        "✘",
                        errs,
                        if errs > 0 {
                            theme::danger()
                        } else {
                            theme::fg_muted()
                        }
                    ),
                    count(
                        "⚠",
                        warns,
                        if warns > 0 {
                            theme::warning()
                        } else {
                            theme::fg_muted()
                        }
                    ),
                ]
                .spacing(8),
            );
        }
        // 1-based line/column of the last click, when there is one.
        if let Some((l, c)) = v.caret {
            let said = tr!("Line {line}, Column {column}", line = l + 1, column = c + 1);
            // As wide as the file's last line, at a column three figures
            // wide, reads: moving the caret moves nothing beside it.
            let widest = super::steady::widest_figure(&tr!(
                "Line {line}, Column {column}",
                line = v.lines.len().max(l + 1),
                column = (c + 1).max(100)
            ));
            bar = bar.push(super::steady::slot(
                status_text(said.clone()),
                &said,
                &[&widest],
                ts::SMALL,
            ));
        }
    }
    // For a file whose dimming depends on the platform (Rust's `#[cfg]`, Go's
    // build constraints, C's `#if`, …), a small target control: read another
    // platform's branches as the live ones. A plain button + our own
    // dropdown, so the label and chevron sit tight together — placed last so
    // the popup anchored to the bottom-right lines up under it.
    if app
        .active_viewer()
        .and_then(|v| v.lang_key)
        .is_some_and(crate::inactive::reads_target)
    {
        // The design's button: 22 high, 6 either side of what it says.
        let picker = button(
            row![
                ts::set_in(
                    text(tr!(
                        "Reading for {target}",
                        target = target_name(&app.proj.reading_target)
                    ))
                    .font(crate::fonts::in_control(crate::fonts::UI_MEDIUM))
                    .color(theme::fg()),
                    PICKER_H,
                    ts::INTER,
                    ts::SMALL,
                ),
                glyph::icon(Glyph::ChevronDown, theme::dim(), 11.0),
            ]
            .spacing(5)
            .align_y(iced::Center),
        )
        .style(theme::icon_button(app.show_target_menu))
        .padding([0, 6])
        .on_press(super::heard::said_with(
            || {
                tr!(
                    "Reading for {target}",
                    target = target_name(&app.proj.reading_target)
                )
            },
            Message::Window(WindowMsg::ToggleTargetMenu),
        ));
        bar = bar.push(picker);
    }

    // The design's 20 px in from the left and 14 from the right, less the
    // padding of the buttons at either end.
    container(bar.spacing(STATUS_GAP).padding(Padding {
        top: 0.0,
        right: 8.0,
        bottom: 0.0,
        left: 12.0,
    }))
    .width(Fill)
    .height(Length::Fixed(STATUSBAR_H))
    .align_y(iced::Center)
    .style(theme::statusbar)
    .into()
}

/// The status bar's offer to set the project's language servers up, in one
/// click, while any it needs still takes something (`App::missing_setup`):
/// a download, an install, or leave to build the project. What the click
/// does is said where the pointer rests. While what it set up is on its
/// way, it says so instead.
/// The height of what the status bar says: one line of its words.
const STATUS_LINE_H: f32 = 20.0;
/// The least room what Clew says is given beside the chips: below it, the
/// chips are said short, and with less still, it is not said.
const SAID_MIN_W: f32 = 120.0;
/// What a chip takes beside its words: its mark, the room after it, and its
/// padding.
const CHIP_EXTRA_W: f32 = 11.0 + 5.0 + 16.0;
/// What the explaining chip takes beside its counter: its mark, its bar,
/// its ×, the room between them.
const EXPLAIN_EXTRA_W: f32 = 11.0 + 90.0 + 19.0 + 3.0 * 5.0;

/// The status bar's left, laid out for the `room` it has: what stays while
/// it lasts — setting up, a pass's progress — and then what Clew says,
/// one line, cut short with an ellipsis. Where the chips and what is said
/// do not fit, the chips are said short (a mark and a count, the whole in
/// their tooltips), and what is said gives way first: a narrow window, or a
/// language of longer words, wrapped the counts onto two lines above the
/// bar, wrapped a chip, and cut what was said mid-word.
fn status_left(app: &App, room: f32) -> Element<'_, Message> {
    let font = crate::fonts::UI;
    let width = |s: &str| super::steady::width_of(s, ts::SMALL, font);
    let setup = setup_label(app);
    let explain = explain_label(app);
    let full_chips: f32 = setup.as_ref().map_or(0.0, |(l, _)| width(l) + CHIP_EXTRA_W)
        + explain
            .as_ref()
            .map_or(0.0, |(_, widest)| width(widest) + EXPLAIN_EXTRA_W);
    let chips_n = usize::from(setup.is_some()) + usize::from(explain.is_some());
    let short = full_chips + STATUS_GAP * chips_n as f32 + SAID_MIN_W > room;
    let mut row = row![].spacing(STATUS_GAP).align_y(iced::Center);
    let mut used = 0.0;
    // A chip short of room even shortened is left out: laid out clipped to
    // nothing, it was a control no one saw that the keyboard's ring still
    // stopped on (its Set up is in the command bar, its pass's progress in
    // the margin).
    if let Some(chip) = setup_chip(app, short).filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    if let Some(chip) = explain_chip(app, short).filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    // What is open, in counts — unless Clew has something to say.
    let said = if !app.status.is_empty() {
        // A status the back end wrote as it is (an error passed straight
        // through) in the reader's language too, its names in the reader's
        // quotation marks, not backticks.
        Some(crate::i18n::quoted(&crate::i18n::message(&app.status)).into_owned())
    } else {
        app.proj.project.as_ref().map(|project| {
            let files = project.files.len();
            let symbols = app.proj.symbol_index.len();
            let mut counts = trn!(
                "{count} file",
                "{count} files",
                files,
                count = thousands(files)
            );
            if symbols > 0 {
                counts.push_str(", ");
                counts.push_str(&trn!(
                    "{count} symbol",
                    "{count} symbols",
                    symbols,
                    count = thousands(symbols)
                ));
            }
            counts
        })
    };
    let left = room - used;
    if let Some(said) = said
        && left >= SAID_MIN_W / 2.0
    {
        let shown = super::steady::cut(&said, left, ts::SMALL, font);
        let line = status_text(shown.clone()).wrapping(iced::widget::text::Wrapping::None);
        row = row.push(if shown == said {
            line.into()
        } else {
            // Cut short: said whole where the pointer rests.
            super::toolbar::tip_at(line, said, None, tooltip::Position::Top)
        });
    }
    container(row)
        .width(Fill)
        .height(Fill)
        .align_y(iced::Center)
        .clip(true)
        .into()
}

/// What the setting-up chip says, and whether it is a button: setting up,
/// or the servers still to set up — `None` when there are none.
fn setup_label(app: &App) -> Option<(String, bool)> {
    if app.setting_up() {
        return Some((tr!("Setting up language servers…").into(), false));
    }
    let missing = app.missing_setup();
    if missing.is_empty() {
        return None;
    }
    Some((
        trn!(
            "Set up {count} language server",
            "Set up {count} language servers",
            missing.len(),
            count = missing.len()
        ),
        true,
    ))
}

/// The setting-up chip, `short` (its mark and count, the whole said in its
/// tooltip) or whole, and its width.
fn setup_chip(app: &App, short: bool) -> Option<(Element<'_, Message>, f32)> {
    let (whole, asks) = setup_label(app)?;
    let label = if !short {
        whole.clone()
    } else if asks {
        app.missing_setup().len().to_string()
    } else {
        "…".to_string()
    };
    let width = super::steady::width_of(&label, ts::SMALL, crate::fonts::UI) + CHIP_EXTRA_W;
    let quiet = row![
        glyph::icon(Glyph::Plug, theme::accent(), 11.0),
        text(label)
            .size(ts::SMALL)
            .color(theme::accent())
            .wrapping(iced::widget::text::Wrapping::None),
    ]
    .spacing(5)
    .align_y(iced::Center);
    if !asks {
        let chip: Element<'_, Message> = if short {
            super::toolbar::tip_at(quiet, whole, None, tooltip::Position::Top)
        } else {
            quiet.into()
        };
        return Some((chip, width));
    }
    let servers: Vec<crate::app::setup::SetupServer> =
        app.missing_setup().into_iter().cloned().collect();
    let tip = if short {
        format!("{whole}\n{}", crate::app::setup::servers_line(&servers))
    } else {
        crate::app::setup::servers_line(&servers)
    };
    // It asks first, as the first open's sheet does: what each server
    // takes — a download, a build that runs the project's own code — is
    // said there, and nothing is fetched or run until it is agreed to.
    let chip = button(quiet)
        .style(theme::icon_button(false))
        .padding([2, 8])
        .on_press(super::heard::said(
            tr!("Set up missing language servers"),
            Message::Project(ProjectMsg::SetupShow(crate::app::setup::SetupMode::Manage)),
        ));
    Some((
        super::toolbar::tip_at(chip, tip, None, tooltip::Position::Top),
        width,
    ))
}

/// A prominent, always-visible progress chip while "Explain All" runs — the
/// pass is slow, so show how far along it is (the status text alone is easy
/// to miss / read as stuck). It is the one place the pass's progress shows:
/// the refresh chip stays out of the way meanwhile (see `refresh_chip`).
///
/// Its counter sits in a slot as wide as it reads at the end of the pass,
/// so the bar and the × beside it stay put as it counts. Calls that failed
/// are not counted here: the pass sends them again, and says what it could
/// not explain once it is done.
/// What the explaining chip's counter says, and the widest it will: `None`
/// while nothing is being explained.
fn explain_label(app: &App) -> Option<(String, String)> {
    if !app.proj.explain.running {
        return None;
    }
    Some(match app.proj.explain.progress {
        Some((done, total)) if total > 0 => {
            let at = |done| tr!("Explaining {done}/{total}", done = done, total = total);
            let widest = [at(total), tr!("Explaining…").to_string()]
                .into_iter()
                .max_by(|a, b| {
                    let w = |s: &str| super::steady::width_of(s, ts::SMALL, crate::fonts::UI);
                    w(a).total_cmp(&w(b))
                })
                .unwrap_or_default();
            (at(done), widest)
        }
        _ => (
            tr!("Explaining…").to_string(),
            tr!("Explaining…").to_string(),
        ),
    })
}

/// The explaining chip, `short` (its counter alone, no word, no bar) or
/// whole, and its width.
fn explain_chip(app: &App, short: bool) -> Option<(Element<'_, Message>, f32)> {
    let (whole, widest_whole) = explain_label(app)?;
    let (label, widest) = match app.proj.explain.progress {
        Some((done, total)) if short && total > 0 => {
            (format!("{done}/{total}"), format!("{total}/{total}"))
        }
        _ if short => ("…".to_string(), "…".to_string()),
        _ => (whole.clone(), widest_whole),
    };
    let font = crate::fonts::UI;
    let counter = super::steady::slot(
        text(label.clone()).size(ts::SMALL).color(theme::accent()),
        &label,
        &[&widest],
        ts::SMALL,
    );
    let mut width = super::steady::width_of(&widest, ts::SMALL, font).max(super::steady::width_of(
        &label,
        ts::SMALL,
        font,
    )) + 11.0
        + 5.0
        + 19.0
        + 5.0;
    // At its own width: squeezed by the counter beside it, the sparkle drew
    // as a sliver two pixels wide.
    let sparkle = container(glyph::icon(Glyph::Sparkle, theme::accent(), 11.0))
        .width(Length::Fixed(11.0))
        .height(Length::Fixed(11.0));
    let mut chip = row![sparkle, counter].spacing(5).align_y(iced::Center);
    // A short determinate bar once the total is known, so progress reads at
    // a glance instead of by parsing the counter.
    if let Some((done, total)) = app.proj.explain.progress
        && total > 0
        && !short
    {
        width += 90.0 + 5.0;
        chip = chip.push(super::meter::eased_bar(
            done as f32 / total as f32,
            90.0,
            4.0,
            theme::progress,
            super::meter::PROGRESS,
        ));
    }
    // A cancel control right on the always-visible chip, so a long pass can
    // be stopped without hunting through menus.
    chip = chip.push(
        button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
            .style(theme::icon_button(false))
            .padding([1, 4])
            .on_press(super::heard::said(
                tr!("Stop explaining"),
                Message::Explain(ExplainMsg::Cancel),
            )),
    );
    let chip: Element<'_, Message> = if short {
        super::toolbar::tip_at(chip, whole, None, tooltip::Position::Top)
    } else {
        chip.into()
    };
    Some((chip, width))
}

/// A freshness indicator for the auto-refreshed understanding: shows whether a
/// refresh is running / queued, and force-refreshes on click (bypassing the 30s
/// auto cooldown). Hidden until there's something to keep fresh (an explanation
/// set exists) and an LLM key is configured — and while an explain pass runs,
/// whose progress chip already says everything this one would (it used to show
/// the same `done/total` a second time).
pub(crate) fn refresh_chip(app: &App) -> Option<Element<'_, Message>> {
    if !app.llm_available || app.proj.explain.cache.is_empty() || app.proj.explain.running {
        return None;
    }
    // (label, colour, clickable). A running pass is shown but not clickable.
    let (label, color, enabled) = if app.proj.overview.generating {
        (
            format!("↻ {}", tr!("Refreshing overview…")),
            theme::accent(),
            false,
        )
    } else if app.proj.building_embeddings {
        (
            format!("↻ {}", tr!("Refreshing index…")),
            theme::accent(),
            false,
        )
    } else if app.proj.refresh_pending {
        // Seconds left before the auto pass fires (a click opens the Explain
        // sheet, as the chip always does).
        let secs = app
            .proj
            .last_auto_refresh
            .map(|t| {
                crate::AUTO_REFRESH_MIN_INTERVAL
                    .saturating_sub(t.elapsed())
                    .as_secs()
                    + 1
            })
            .unwrap_or(0);
        (
            format!("↻ {}", tr!("Refresh in {seconds}s", seconds = secs)),
            theme::accent(),
            true,
        )
    } else if !app.keep_explanations_fresh && !app.proj.explain.changed_sources.is_empty() {
        // Code changed, and nothing refreshes it by itself (Settings →
        // Usage and cost): said, not claimed up to date.
        (
            format!("↻ {}", tr!("Explanations out of date")),
            theme::accent(),
            true,
        )
    } else {
        // The notes, not the project: "Up to date" beside the scan's own
        // "Up to date" said one word for two things.
        (
            format!("↻ {}", tr!("Explanations up to date")),
            theme::dim(),
            true,
        )
    };
    // Its words in a slot as wide as the widest of them: counting down, and
    // turning from one to the next, it moved everything to its right.
    let every = [
        format!("↻ {}", tr!("Refreshing overview…")),
        format!("↻ {}", tr!("Refreshing index…")),
        format!(
            "↻ {}",
            tr!(
                "Refresh in {seconds}s",
                seconds = crate::AUTO_REFRESH_MIN_INTERVAL.as_secs()
            )
        ),
        format!("↻ {}", tr!("Explanations up to date")),
        format!("↻ {}", tr!("Explanations out of date")),
    ];
    let every: Vec<&str> = every.iter().map(String::as_str).collect();
    let words = super::steady::slot(
        text(label.clone()).size(ts::SMALL).color(color),
        &label,
        &every,
        ts::SMALL,
    );
    let mut b = button(words)
        .style(theme::icon_button(false))
        .padding([2, 8]);
    if enabled {
        // As Explain All is: the sheet first, with what it would cost. A
        // click on a label that read "up to date" ran the whole pass, every
        // function not yet explained included, with no word of its cost.
        b = b.on_press(super::heard::said(
            &label,
            super::toolbar::explain_all_message(app),
        ));
        return Some(super::toolbar::tip_at(
            b,
            tr!("Explain what's new or changed. Shows the cost before anything runs."),
            None,
            tooltip::Position::Top,
        ));
    }
    Some(b.into())
}

/// What a file no grammar reads is, said by its name: prose in Markdown —
/// a README — is Markdown, not "Plain text".
fn untagged_kind(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("md" | "markdown" | "mdx") => "Markdown",
        Some("rst") => "reStructuredText",
        _ => tr!("Plain text"),
    }
}

#[cfg(test)]
mod target_name_tests {
    use super::target_name;
    use crate::inactive::Target;

    /// A target reads as its system, its architecture said only where it
    /// is not that system's usual one; the machine's own as its system.
    #[test]
    fn a_target_is_named_by_its_system() {
        let named = |label: &str| target_name(&Target::from_label(label));
        assert_eq!(named("Windows (x86_64)"), "Windows");
        assert_eq!(named("Windows (arm64)"), "Windows (arm64)");
        assert_eq!(named("Linux (x86_64)"), "Linux");
        assert_eq!(named("macOS (arm64)"), "macOS");
        assert_eq!(named("macOS (x86_64)"), "macOS (x86_64)");
        assert_eq!(named("iOS (arm64)"), "iOS");
        assert_eq!(named("Android (arm64)"), "Android");
        assert_eq!(named("Web (wasm32)"), "Web");
        let host = Target::host();
        assert!(!target_name(&host).contains('('), "{}", target_name(&host));
    }
}

#[cfg(test)]
mod untagged_kind_tests {
    use super::untagged_kind;
    use std::path::Path;

    /// A README says it is Markdown; a file of no known kind is plain text.
    #[test]
    fn a_readme_is_markdown() {
        assert_eq!(untagged_kind(Path::new("/p/README.md")), "Markdown");
        assert_eq!(untagged_kind(Path::new("/p/docs/guide.MD")), "Markdown");
        assert_eq!(untagged_kind(Path::new("/p/notes.txt")), "Plain text");
        assert_eq!(untagged_kind(Path::new("/p/LICENSE")), "Plain text");
    }
}
