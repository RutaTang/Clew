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
        tr!("Indexed")
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
                &[tr!("Scanning…"), tr!("Indexing…"), tr!("Indexed")],
                ts::SMALL,
            );
            // What is up to date: the Updates page's "Clew is up to date"
            // is of the app, this of the project's files.
            if state == tr!("Indexed") {
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
    // Where the pointer is on it, a failure standing there holds still
    // (`FailureNotice::held`).
    let said_row = mouse_area(
        container(iced::widget::responsive(move |room| {
            status_left(app, room.width)
        }))
        .width(Fill)
        .height(Length::Fixed(STATUS_LINE_H)),
    )
    .on_enter(Message::Window(WindowMsg::StatusHover(true)))
    .on_exit(Message::Window(WindowMsg::StatusHover(false)));
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
            (slot, (shown != said).then(|| said.clone()))
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
            let press = super::heard::said(
                tr!("Language servers"),
                Message::Settings(SettingsMsg::OpenAt(crate::SettingsPage::Languages)),
            );
            // Named on the ring by what it shows, then where it leads:
            // "Python: pyright ready" was announced "Language servers".
            lsp_button = lsp_button.on_press(super::heard::said_with(
                || format!("{said} — {}", tr!("Language servers")),
                press,
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
                        "Built for {target}",
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
                    "Built for {target}",
                    target = target_name(&app.proj.reading_target)
                )
            },
            Message::Window(WindowMsg::ToggleTargetMenu),
        ));
        // "Reading for Linux" said nothing of what it changes: named by
        // what the code is read as, and what that does said where the
        // pointer rests.
        bar = bar.push(super::toolbar::tip_at(
            picker,
            tr!("Code built only for other platforms is dimmed"),
            None,
            tooltip::Position::Top,
        ));
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
/// The most room a standing failure gives what Clew says beside it — half
/// the bar, up to this, whatever is said: enough for what is under way to
/// be said whole ("Working out what discounts does…" was cut to "Working
/// out wh…" beside an older failure given the rest). With nothing said
/// beside it, the failure has the room, and is said whole where it fits:
/// kept empty for what might be said, it was cut to "Oldest TODO did not
/// finish…" beside nothing.
const SAID_KEPT_MAX_W: f32 = 320.0;
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
    let mut row = row![].spacing(STATUS_GAP).align_y(iced::Center);
    // A status the back end wrote as it is (an error passed straight
    // through) in the reader's language too, its names in the reader's
    // quotation marks, not backticks.
    let words_of = |s: &str| crate::i18n::quoted(&crate::i18n::message(s)).into_owned();
    let least = SAID_MIN_W / 2.0;
    // Words cut to `room`, said whole where the pointer rests when cut.
    let line_of = |whole: String, room: f32, color: Option<iced::Color>| -> Element<'_, Message> {
        let shown = super::steady::cut(&whole, room, ts::SMALL, font);
        let mut line = status_text(shown.clone()).wrapping(iced::widget::text::Wrapping::None);
        if let Some(color) = color {
            line = line.color(color);
        }
        if shown == whole {
            line.into()
        } else {
            super::toolbar::tip_at(line, whole, None, tooltip::Position::Top)
        }
    };
    // A failure stands first, in the danger colour, with what to do about
    // it — what fits of it beside the least of its words, its dismissal
    // always. It gives room to what is said beside it, and takes it back
    // when that goes; but not while the pointer is on the line: after the
    // chips, and shortened to make room for the next message, its Try
    // again, Settings and × moved under the pointer, and a click meant for
    // Settings landed on words (`FailureNotice::held`).
    let failure = app.failure_shown();
    let setup = setup_label(app);
    let explain = explain_label(app);
    // A plugin's command running is laid out with the other chips: laid
    // out after a failure standing, with what room it left, its Stop was
    // gone beside a long one, and nothing said a command ran.
    let run_chip = plugin_run_chip(app);
    let full_chips: f32 = run_chip.as_ref().map_or(0.0, |chip| chip.1)
        + setup.as_ref().map_or(0.0, |(l, _)| width(l) + CHIP_EXTRA_W)
        + explain.as_ref().map_or(0.0, |(_, widest)| {
            // One node's chip has no bar: counted with one, it gave way
            // to "…" with room to spare.
            let bar = if app.proj.explain.pass_target.is_some() {
                90.0 + 5.0
            } else {
                0.0
            };
            width(widest) + EXPLAIN_EXTRA_W - bar
        });
    let chips_n = usize::from(setup.is_some())
        + usize::from(explain.is_some())
        + usize::from(run_chip.is_some());
    // What is under way now comes before an older failure: its chip keeps
    // its words, and the failure's give way (whole where the pointer
    // rests). Beside a run's failure, Explain's progress was squeezed to
    // "✦ … ×".
    let chips_w = full_chips + STATUS_GAP * chips_n as f32;
    // What the offer to set servers up gives back said short — its mark
    // and count, the whole where the pointer rests. An offer waits; what
    // failed, beside it, was cut to "Oldest TO…" with the offer said whole.
    let setup_saving = match &setup {
        Some((whole, true)) => {
            (width(whole) - width(&app.missing_setup().len().to_string())).max(0.0)
        }
        _ => 0.0,
    };
    let mut used = 0.0;
    if let Some(failure) = failure
        && room >= least
    {
        let whole = words_of(&failure.shown());
        let (controls, controls_w) = failure_controls(failure, room - least - STATUS_GAP);
        // What failed: its words before the reason — ": ", or the
        // full-width "：" Chinese and Japanese write.
        let head = failure_head(&whole, failure.subject.as_deref());
        // While the codes are up, what to do with them is said whole —
        // after the failure's words it was cut to "Type the code on a…" —
        // and what failed is said before its controls, in what room is
        // left: without it, "Try again  Settings ×" stood with nothing
        // said of what they were about. Its controls stay, each with its
        // code; the whole failure is where the pointer rests.
        if app.jump.is_some() {
            let hint_w = width(&words_of(&app.status));
            let free = room - controls_w - 3.0 * STATUS_GAP - chips_w - hint_w;
            let words = width(head).min(free);
            if words >= least {
                let shown = super::steady::cut(head, words, ts::SMALL, font);
                let line = status_text(shown)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .color(theme::danger());
                row = row.push(super::toolbar::tip_at(
                    line,
                    whole.clone(),
                    None,
                    tooltip::Position::Top,
                ));
                used += words + STATUS_GAP;
            }
            row = row.push(controls);
            used += controls_w + STATUS_GAP;
        } else {
            // With something said beside it, half of what is free is kept
            // for that, up to `SAID_KEPT_MAX_W`; the failure keeps at least
            // what failed (its words before the reason), its reason whole
            // where the pointer rests. With nothing, all of it is the
            // failure's. Laid out as it was when the pointer came, while
            // the pointer is on the line.
            // What failed counts on the offer to set up said short, where
            // it would not be said whole beside the offer said whole — and,
            // with nothing beside it, so does its reason: an offer waits.
            let beside = failure.held.unwrap_or_else(|| app.said_beside_failure());
            let least_beside = if beside { SAID_MIN_W } else { 0.0 };
            let head_w = width(head) + width("…");
            let needs = if beside { head_w } else { width(&whole) };
            let free = room - controls_w - 2.0 * STATUS_GAP - chips_w;
            let free = match needs > free - least_beside {
                true => free + setup_saving,
                false => free,
            };
            let kept = if beside {
                (free / 2.0).clamp(SAID_MIN_W, SAID_KEPT_MAX_W)
            } else {
                0.0
            };
            let floor = head_w.min(free - least_beside);
            let words = (free - kept).max(floor).max(least).min(width(&whole));
            row = row.push(line_of(whole, words, Some(theme::danger())));
            row = row.push(controls);
            used += words + STATUS_GAP + controls_w + STATUS_GAP;
        }
    }
    let room = (room - used).max(0.0);
    let mut used = 0.0;
    // What Clew says besides a failure that stands: the failure keeps its
    // own place, and what is said after it (a file copied, a search done)
    // goes beside it rather than over it — said over it, the red Explain
    // failure was gone at the next word, its Try again with it.
    // The failure's own words are not said again: not beside it, nor in grey
    // while it is not shown here (an Ask answer's, with Ask open, said in
    // the conversation).
    let own_words = app
        .failure_notice
        .as_ref()
        .is_some_and(|f| f.said == app.status);
    let said = if !app.status.is_empty() && !own_words {
        Some(words_of(&app.status))
    } else if failure.is_some() {
        None
    } else {
        // What is open, in counts — unless Clew has something to say.
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
    // The offer to set servers up is said short first — for the least room
    // what is said needs, and for what is said to be said whole: an offer
    // waits, what was just said goes ("TODO count is installed: its
    // commands are in the comma…" beside the offer said whole) — and what
    // is under way (Explain's pass) only when even that leaves too little.
    // Nothing said, nothing is made room for: beside a failure said whole,
    // the offer was said short for the room of words that were not there.
    let gaps = STATUS_GAP * chips_n as f32;
    let said_w = said.as_deref().map_or(0.0, width);
    let said_least = if said.is_some() { SAID_MIN_W } else { 0.0 };
    let setup_short = full_chips + gaps + said_least.max(said_w) > room;
    let short =
        full_chips - if setup_short { setup_saving } else { 0.0 } + gaps + said_least > room;
    // A chip short of room even shortened is left out: laid out clipped to
    // nothing, it was a control no one saw that the keyboard's ring still
    // stopped on (its Set up is in the command bar, its pass's progress in
    // the margin).
    if let Some(chip) = setup_chip(app, setup_short).filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    if let Some(chip) = explain_chip(app, short).filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    // A session going on behind the hidden panel, said, and a way back to
    // it: hidden while paused, nothing showed one was live, and Debug
    // started a second copy of the program.
    if let Some(chip) = session_chip(app).filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    if let Some(chip) = run_chip.filter(|chip| used + chip.1 <= room) {
        used += chip.1 + STATUS_GAP;
        row = row.push(chip.0);
    }
    let left = room - used;
    if let Some(said) = said
        && left >= least
    {
        row = row.push(line_of(said, left, None));
    }
    container(row)
        .width(Fill)
        .height(Fill)
        .align_y(iced::Center)
        .clip(true)
        .into()
}

/// What failed, of a failure's words: its words before the reason — ": ",
/// or the full-width "：" Chinese and Japanese write — after what failed
/// where its words name it (`subject`): a plugin's command is named "TODO
/// count: List TODOs", and cut at its own ": " what failed read "TODO
/// count".
pub(crate) fn failure_head<'a>(whole: &'a str, subject: Option<&str>) -> &'a str {
    let after = subject
        .filter(|s| !s.is_empty())
        .and_then(|s| whole.find(s).map(|at| at + s.len()))
        .unwrap_or(0);
    [": ", "："]
        .iter()
        .filter_map(|sep| whole[after..].find(sep).map(|at| after + at))
        .min()
        .map_or(whole, |at| &whole[..at])
}

/// What a failure on the status line offers (`App::failure_notice`): try
/// it again, where it can be; the settings it may be about; and a way to
/// take it off the line — what fits in `room` of them, the way to take it
/// off the line always (clipped away, it was a control no one saw, and the
/// failure stood for good). With their width.
fn failure_controls(
    failure: &crate::app::state::FailureNotice,
    room: f32,
) -> (Element<'_, Message>, f32) {
    let font = crate::fonts::UI_MEDIUM;
    let link = |label: &str, msg: Message| {
        button(
            text(label.to_string())
                .size(ts::SMALL)
                .font(font)
                .color(theme::accent())
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .style(theme::icon_button(false))
        .padding([1, 6])
        .on_press(super::heard::said(label, msg))
    };
    let link_w = |label: &str| super::steady::width_of(label, ts::SMALL, font) + 12.0 + 2.0;
    // A target the pointer finds: the glyph alone was 8 px of it.
    let dismiss_w = 11.0 + 14.0;
    // Where trying again asks first — Explain All's cost, a key — it says
    // so, as a menu's row that opens a dialog does: "Try again" opened the
    // cost sheet where it read as done at once.
    let asks = failure.asks_first();
    let again = if asks {
        tr!("Try again…")
    } else {
        tr!("Try again")
    };
    let retry_w = failure.retry.as_ref().map_or(0.0, |_| link_w(again));
    let settings_w = link_w(tr!("Settings"));
    // Settings gives way first, then Try again.
    let with_retry = retry_w > 0.0 && dismiss_w + retry_w <= room;
    let kept_retry = if with_retry { retry_w } else { 0.0 };
    let with_settings = dismiss_w + kept_retry + settings_w <= room;
    let mut row = row![].spacing(2).align_y(iced::Center);
    let mut width = 0.0;
    if with_retry {
        row = row.push(link(again, Message::Window(WindowMsg::RetryFailure)));
        width += retry_w;
    }
    if with_settings {
        // At the field it is about, where it is about one.
        // At what it is about there: a plugin's row (its log, what fixes
        // it), or Rust's setup.
        row = row.push(link(
            tr!("Settings"),
            match (failure.about, &failure.spot) {
                (Some(field), _) => Message::Settings(SettingsMsg::Fix(field)),
                (None, Some(spot)) => {
                    Message::Plugin(crate::app::plugins::PluginMsg::ShowInSettings(spot.clone()))
                }
                (None, None) => Message::Settings(SettingsMsg::OpenAt(failure.of.settings_page())),
            },
        ));
        width += settings_w;
    }
    let dismiss = button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
        .style(theme::icon_button(false))
        .padding([5, 7])
        .on_press(super::heard::said(
            tr!("Dismiss"),
            Message::Window(WindowMsg::DismissFailure),
        ));
    row = row.push(super::toolbar::tip_at(
        dismiss,
        tr!("Dismiss"),
        None,
        tooltip::Position::Top,
    ));
    width += dismiss_w;
    (row.into(), width)
}

/// The chip of a run or debug session going on behind the hidden bottom
/// panel — "Debugging main.py — paused" — which shows the panel again; and
/// its width. `None` while the panel shows, or nothing is live.
fn session_chip(app: &App) -> Option<(Element<'_, Message>, f32)> {
    if !app.hidden_session_live() {
        return None;
    }
    let session = app.debug.session.as_ref()?;
    // A remote run's program is the project's folder there, not a file: no
    // name (as the status line says it), where it said "Debugging Clew".
    let file = Some(&session.program)
        .filter(|_| !app.connection.is_remote())
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    let paused = session.status == crate::DebugStatus::Stopped;
    let said = match (app.debug.no_debug, paused, file) {
        (true, _, Some(file)) => tr!("Running {file}", file = file),
        (false, true, Some(file)) => tr!("Debugging {file} — paused", file = file),
        (false, false, Some(file)) => tr!("Debugging {file}", file = file),
        (true, _, None) => tr!("Running").to_string(),
        (false, true, None) => tr!("Debugging — paused").to_string(),
        (false, false, None) => tr!("Debugging").to_string(),
    };
    let width = super::steady::width_of(&said, ts::SMALL, crate::fonts::UI) + CHIP_EXTRA_W;
    let chip = button(
        row![
            glyph::icon(Glyph::Debug, theme::accent(), 11.0),
            text(said.clone())
                .size(ts::SMALL)
                .color(theme::accent())
                .wrapping(iced::widget::text::Wrapping::None),
        ]
        .spacing(5)
        .align_y(iced::Center),
    )
    .style(theme::icon_button(false))
    .padding([2, 8])
    .on_press(super::heard::said(
        &said,
        Message::Window(WindowMsg::ShowDebugPanel),
    ));
    Some((
        super::toolbar::tip_at(
            chip,
            tr!("Show the Run/Debug panel"),
            None,
            tooltip::Position::Top,
        ),
        width,
    ))
}

/// The most of its width a running command's name takes on its chip: a
/// long one left no room for its Stop.
const RUN_NAME_MAX_W: f32 = 200.0;

/// A plugin's command running: its name, how far it has come when it says,
/// and its Stop, for as long as it runs. Said on the status line, it went as
/// soon as anything else was said, and could not be stopped.
fn plugin_run_chip(app: &App) -> Option<(Element<'_, Message>, f32)> {
    let (title, percent, stopping) = app.plugin_run()?;
    let font = crate::fonts::UI;
    let name = super::steady::cut(title, RUN_NAME_MAX_W, ts::SMALL, font);
    let said = match (stopping, percent) {
        (true, _) => tr!("Stopping {command}…", command = name.clone()),
        (false, Some(p)) => tr!(
            "Running {command}… {percent}%",
            command = name.clone(),
            percent = p
        ),
        (false, None) => tr!("Running {command}…", command = name.clone()),
    };
    // At its widest once it counts: it does not shift as the count grows.
    // One that never counts keeps no room for a count (its Stop stood
    // apart from its words).
    let widest = match percent {
        Some(_) if !stopping => tr!(
            "Running {command}… {percent}%",
            command = name.clone(),
            percent = 100
        ),
        _ => said.clone(),
    };
    let words_w = super::steady::width_of(&widest, ts::SMALL, font).max(super::steady::width_of(
        &said,
        ts::SMALL,
        font,
    ));
    let mut width = 11.0 + 5.0 + words_w;
    let mut chip = row![
        // Plugins' own mark (Settings → Plugins): the plug beside it is
        // the language servers' setup.
        container(glyph::icon(Glyph::Cube, theme::accent(), 11.0))
            .width(Length::Fixed(11.0))
            .height(Length::Fixed(11.0)),
        container(
            text(said)
                .size(ts::SMALL)
                .color(theme::accent())
                .wrapping(iced::widget::text::Wrapping::None),
        )
        .width(Length::Fixed(words_w)),
    ]
    .spacing(5)
    .align_y(iced::Center);
    // Asked to stop, it is stopping: no second Stop.
    if !stopping {
        width += 5.0 + 19.0;
        let stop = tr!("Stop {command}", command = title.to_string());
        chip = chip.push(super::toolbar::tip_at(
            button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
                .style(theme::icon_button(false))
                .padding([1, 4])
                .on_press(super::heard::said(
                    &stop,
                    Message::Plugin(crate::app::plugins::PluginMsg::StopRun),
                )),
            stop.clone(),
            None,
            tooltip::Position::Top,
        ));
    }
    let chip: Element<'_, Message> = if name == title {
        chip.into()
    } else {
        super::toolbar::tip_at(chip, title.to_string(), None, tooltip::Position::Top)
    };
    Some((chip, width))
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
    // One function or file asked for: named, not counted against the
    // whole project — "Explaining 0/45" beside an Explain that said "one
    // AI call" read as 45 paid calls started.
    if let Some(node) = &app.proj.explain.pass_target {
        let said = tr!("Explaining {name}…", name = node_name(node));
        return Some((said.clone(), said));
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

#[cfg(test)]
pub(crate) fn explain_label_for_tests(app: &App) -> Option<(String, String)> {
    explain_label(app)
}

/// A node as the explaining chip names it: a function with its brackets,
/// a file or folder by its own name.
fn node_name(node: &crate::explain::Node) -> String {
    use crate::explain::Node;
    match node {
        Node::Function { name, .. } => format!("{name}()"),
        Node::File(path) | Node::Folder(path) => path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        ),
    }
}

/// The explaining chip, `short` (its counter alone, no word, no bar) or
/// whole, and its width.
fn explain_chip(app: &App, short: bool) -> Option<(Element<'_, Message>, f32)> {
    let (whole, widest_whole) = explain_label(app)?;
    // One node's pass has no count to show: its progress is the project's.
    let progress = app
        .proj
        .explain
        .progress
        .filter(|_| app.proj.explain.pass_target.is_none());
    let (label, widest) = match progress {
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
    if let Some((done, total)) = progress
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
    chip = chip.push(super::toolbar::tip_at(
        button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
            .style(theme::icon_button(false))
            .padding([1, 4])
            .on_press(super::heard::said(
                tr!("Stop explaining"),
                Message::Explain(ExplainMsg::Cancel),
            )),
        tr!("Stop explaining (what is explained so far is kept)"),
        None,
        tooltip::Position::Top,
    ));
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
        // What it knows: nothing changed since the explanations were
        // written. "Explanations up to date" read as all of the project
        // explained, beside a card saying "Not explained yet."
        (
            format!("↻ {}", tr!("No changes to explain")),
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
        format!("↻ {}", tr!("No changes to explain")),
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
        // Named for what it does, Explain All, not for the state it shows:
        // "No changes to explain" was listed in the command bar as a
        // command, and led to a paid pass under a state's name.
        b = b.on_press(super::heard::said(
            tr!("Explain All"),
            super::toolbar::explain_all_message(app),
        ));
        return Some(super::toolbar::tip_at(
            b,
            tr!(
                "Explain what's new, changed or not yet explained. Shows the cost before anything runs."
            ),
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
    fn what_failed_is_cut_after_its_own_name() {
        use super::failure_head;
        let whole = "TODO count: List TODOs did not finish: it crashed: boom";
        assert_eq!(failure_head(whole, None), "TODO count");
        assert_eq!(
            failure_head(whole, Some("TODO count: List TODOs")),
            "TODO count: List TODOs did not finish"
        );
        // Named nowhere in its words (another language's): as before.
        assert_eq!(failure_head(whole, Some("absent")), "TODO count");
        assert_eq!(
            failure_head("清单：列出待办 未完成：崩溃", Some("清单：列出待办")),
            "清单：列出待办 未完成"
        );
    }

    #[test]
    fn a_readme_is_markdown() {
        assert_eq!(untagged_kind(Path::new("/p/README.md")), "Markdown");
        assert_eq!(untagged_kind(Path::new("/p/docs/guide.MD")), "Markdown");
        assert_eq!(untagged_kind(Path::new("/p/notes.txt")), "Plain text");
        assert_eq!(untagged_kind(Path::new("/p/LICENSE")), "Plain text");
    }
}
