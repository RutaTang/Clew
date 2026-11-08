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
        _ => return target.label.clone(),
    };
    let usual = if target.os == "macos" {
        "aarch64"
    } else {
        "x86_64"
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
    // Scanning, indexing, up to date: in a slot as wide as the widest of
    // them, so what Clew says beside it stays where it is.
    let said_place = match sync_state(app) {
        Some(state) => format!("{place} · {state}"),
        None => place.clone(),
    };
    let every_state: Vec<String> = [tr!("Scanning…"), tr!("Indexing…"), tr!("Up to date")]
        .into_iter()
        .map(|state| format!("{place} · {state}"))
        .collect();
    let every_state: Vec<&str> = every_state.iter().map(String::as_str).collect();
    let place_text = super::steady::slot(
        text(said_place.clone()).size(ts::SMALL).color(if remote {
            theme::accent()
        } else {
            theme::fg_muted()
        }),
        &said_place,
        if app.proj.project.is_some() {
            &every_state
        } else {
            &[]
        },
        ts::SMALL,
    );
    let conn_indicator = super::tip::tip(
        button(row![dot, place_text,].spacing(7).align_y(iced::Center))
            .style(theme::icon_button(false))
            .padding([2, 8])
            .on_press(Message::Connect(ConnectMsg::Open)),
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

    // What is open, in counts — unless Clew has something to say.
    let said: Element<'_, Message> = if !app.status.is_empty() {
        // A status the back end wrote as it is (an error passed straight
        // through) in the reader's language too.
        status_text(crate::i18n::message(&app.status))
            .wrapping(iced::widget::text::Wrapping::None)
            .into()
    } else if let Some(project) = app.proj.project.as_ref() {
        let files = project.files.len();
        let symbols = app.proj.symbol_index.len();
        let mut counts = trn!(
            "{count} file",
            "{count} files",
            files,
            count = thousands(files)
        );
        if symbols > 0 {
            counts.push_str(" · ");
            counts.push_str(&trn!(
                "{count} symbol",
                "{count} symbols",
                symbols,
                count = thousands(symbols)
            ));
        }
        status_text(counts).into()
    } else {
        slot()
    };
    // The design's gap after where the code is read from, less the padding
    // of the button that says it — a row of its own, since the bar's own
    // spacing (the design's gap between everything else) is one for all.
    //
    // What stays while it lasts — setting up, a pass's progress — comes
    // first, and what Clew says last, taking the room they leave, cut off at
    // its end: a long message (a path, an error) laid out first took the
    // whole bar, the caret's place was wrapped down the window's edge a word
    // a line, and a chip after it moved with every word said.
    let mut said_row = row![].spacing(STATUS_GAP).align_y(iced::Center);
    if let Some(chip) = setup_chip(app) {
        said_row = said_row.push(chip);
    }
    if let Some(chip) = explain_chip(app) {
        said_row = said_row.push(chip);
    }
    said_row = said_row.push(said);
    let mut bar = row![
        row![conn_indicator, container(said_row).width(Fill).clip(true)]
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
        let short: String = tt
            .commits
            .get(tt.idx)
            .map(|c| c.sha.chars().take(8).collect())
            .unwrap_or_default();
        let scope = match &tt.scope {
            TimeScope::Symbol { name, kind, .. } => format!("  ·  {} {name}", short_kind(kind)),
            TimeScope::File => String::new(),
        };
        let lines = tt
            .viewer
            .as_ref()
            .map(|v| format!("  ·  {}", trn!("1 line", "{n} lines", v.lines.len())))
            .unwrap_or_default();
        let said = format!(
            "{}  ·  {short}  ·  {}/{}{}{}",
            tr!("Time Travel"),
            tt.idx + 1,
            tt.commits.len(),
            scope,
            lines
        );
        // At its widest: the last revision's number, and a line count a
        // digit longer than this revision's.
        let widest = super::steady::widest_figure(&format!(
            "{}  ·  {short}  ·  {}/{}{}{}",
            tr!("Time Travel"),
            tt.commits.len(),
            tt.commits.len(),
            scope,
            format!("{lines}0"),
        ));
        bar = bar.push(super::steady::slot(
            status_text(said.clone()),
            &said,
            &[&widest],
            ts::SMALL,
        ));
    } else if let Some(v) = app.active_viewer() {
        let lang = v
            .lang_key
            .and_then(crate::highlight::lang_name)
            .unwrap_or(tr!("Plain text"));
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
            Some((Some(name), _)) => format!("{lang} · {name} {status}"),
            Some((None, _)) => format!("{lang} · {status}"),
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
        let lsp_text: Element<'_, Message> = if busy {
            let width =
                super::steady::widest(&[&at_rest[0], &at_rest[1]], ts::SMALL, crate::fonts::UI)
                    .max(LSP_BUSY_W);
            let said = super::steady::cut(&said, width, ts::SMALL, crate::fonts::UI);
            container(status_text(said).wrapping(iced::widget::text::Wrapping::None))
                .width(width)
                .clip(true)
                .into()
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
            if shown == said {
                slot
            } else {
                super::toolbar::tip_at(slot, said, None, tooltip::Position::Top)
            }
        };
        bar = bar.push(lsp_text);
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
        .on_press(Message::Window(WindowMsg::ToggleTargetMenu));
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
fn setup_chip(app: &App) -> Option<Element<'_, Message>> {
    let quiet = |label: String| {
        row![
            glyph::icon(Glyph::Plug, theme::accent(), 11.0),
            text(label).size(ts::SMALL).color(theme::accent()),
        ]
        .spacing(5)
        .align_y(iced::Center)
    };
    if app.setting_up() {
        return Some(quiet(tr!("Setting up language servers…").into()).into());
    }
    let missing = app.missing_setup();
    if missing.is_empty() {
        return None;
    }
    let servers: Vec<crate::app::setup::SetupServer> = missing.into_iter().cloned().collect();
    let label = trn!(
        "Set up {count} language server",
        "Set up {count} language servers",
        servers.len(),
        count = servers.len()
    );
    let chip = button(quiet(label))
        .style(theme::icon_button(false))
        .padding([2, 8])
        .on_press(Message::Project(ProjectMsg::SetUpMissing));
    Some(super::toolbar::tip_at(
        chip,
        crate::app::setup::servers_line(&servers),
        None,
        tooltip::Position::Top,
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
fn explain_chip(app: &App) -> Option<Element<'_, Message>> {
    if !app.proj.explain.running {
        return None;
    }
    let (label, widest) = match app.proj.explain.progress {
        Some((done, total)) if total > 0 => {
            let at = |done| tr!("Explaining {done}/{total}", done = done, total = total);
            (at(done), vec![at(total), tr!("Explaining…").to_string()])
        }
        _ => (
            tr!("Explaining…").to_string(),
            vec![tr!("Explaining…").to_string()],
        ),
    };
    let widest: Vec<&str> = widest.iter().map(String::as_str).collect();
    let counter = super::steady::slot(
        text(label.clone()).size(ts::SMALL).color(theme::accent()),
        &label,
        &widest,
        ts::SMALL,
    );
    let mut chip = row![glyph::icon(Glyph::Sparkle, theme::accent(), 11.0), counter]
        .spacing(5)
        .align_y(iced::Center);
    // A short determinate bar once the total is known, so progress reads at
    // a glance instead of by parsing the counter.
    if let Some((done, total)) = app.proj.explain.progress
        && total > 0
    {
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
            .on_press(Message::Explain(ExplainMsg::Cancel)),
    );
    Some(chip.into())
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
        // Seconds left before the auto pass fires (click to skip the wait).
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
    } else {
        (format!("↻ {}", tr!("Up to date")), theme::dim(), true)
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
        format!("↻ {}", tr!("Up to date")),
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
        b = b.on_press(Message::Explain(ExplainMsg::RefreshAll));
    }
    Some(b.into())
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
        let host = Target::host();
        assert!(!target_name(&host).contains('('), "{}", target_name(&host));
    }
}
