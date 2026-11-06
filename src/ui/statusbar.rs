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
    let conn_indicator = tooltip(
        button(
            row![
                dot,
                text(match sync_state(app) {
                    Some(state) => format!("{place} · {state}"),
                    None => place,
                })
                .size(ts::SMALL)
                .color(if remote {
                    theme::accent()
                } else {
                    theme::fg_muted()
                }),
            ]
            .spacing(7)
            .align_y(iced::Center),
        )
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
    );

    // What is open, in counts — unless Clew has something to say.
    let said: Element<'_, Message> = if !app.status.is_empty() {
        // A status the back end wrote as it is (an error passed straight
        // through) in the reader's language too.
        status_text(crate::i18n::message(&app.status)).into()
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
    let mut bar = row![
        row![conn_indicator, said]
            .spacing(STATUS_GAP - 8.0)
            .align_y(iced::Center)
    ]
    .align_y(iced::Center);
    // A prominent, always-visible progress chip while "Explain All" runs — the
    // pass is slow, so show how far along it is (the status text alone is easy
    // to miss / read as stuck). It is the one place the pass's progress shows:
    // the refresh chip stays out of the way meanwhile (see `refresh_chip`).
    if app.proj.explain.running {
        let label = match app.proj.explain.progress {
            Some((done, total)) if total > 0 => {
                tr!("Explaining {done}/{total}", done = done, total = total)
            }
            _ => tr!("Explaining…").to_string(),
        };
        let mut chip = row![
            glyph::icon(Glyph::Sparkle, theme::accent(), 11.0),
            text(label).size(ts::SMALL).color(theme::accent()),
        ]
        .spacing(5)
        .align_y(iced::Center);
        // A short determinate bar once the total is known, so progress reads at a
        // glance instead of by parsing the counter.
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
        // Failures never hide behind the counter: a running tally in warn red.
        if app.proj.explain.failed > 0 {
            chip = chip.push(
                text(format!(
                    "· {}",
                    trn!("1 failed", "{n} failed", app.proj.explain.failed)
                ))
                .size(ts::SMALL)
                .color(theme::warn()),
            );
        }
        // A cancel control right on the always-visible chip, so a long pass can
        // be stopped without hunting through menus.
        chip = chip.push(
            button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
                .style(theme::icon_button(false))
                .padding([1, 4])
                .on_press(Message::Explain(ExplainMsg::Cancel)),
        );
        bar = bar.push(chip);
    }
    bar = bar.push(space().width(Fill));
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
        bar = bar.push(status_text(format!(
            "{}  ·  {short}  ·  {}/{}{}{}",
            tr!("Time Travel"),
            tt.idx + 1,
            tt.commits.len(),
            scope,
            lines
        )));
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
        let lsp = v
            .lang_key
            .and_then(|k| {
                let status = app.lsp_status(k)?;
                let at_work = matches!(
                    app.proj.link.lsp.get(k),
                    Some(crate::LspSlot::Ready(_) | crate::LspSlot::Starting)
                );
                Some(
                    match crate::lsp::registry::default_for_language(k).filter(|_| at_work) {
                        Some(server) => format!(" · {} {status}", server.name),
                        None => format!(" · {status}"),
                    },
                )
            })
            .unwrap_or_default();
        bar = bar.push(status_text(format!("{lang}{lsp}")));
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
        if let Some((errs, warns)) = diags
            && errs + warns > 0
        {
            bar = bar.push(
                row![
                    text(format!("✘ {errs}"))
                        .size(ts::SMALL)
                        .color(if errs > 0 {
                            theme::danger()
                        } else {
                            theme::fg_muted()
                        }),
                    text(format!("⚠ {warns}"))
                        .size(ts::SMALL)
                        .color(if warns > 0 {
                            theme::warning()
                        } else {
                            theme::fg_muted()
                        }),
                ]
                .spacing(8),
            );
        }
        // 1-based line/column of the last click, when there is one.
        if let Some((l, c)) = v.caret {
            bar = bar.push(status_text(tr!(
                "Line {line}, Column {column}",
                line = l + 1,
                column = c + 1
            )));
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
            format!("↻ {}", tr!("Update queued · {seconds}s", seconds = secs)),
            theme::accent(),
            true,
        )
    } else {
        (format!("↻ {}", tr!("Up to date")), theme::dim(), true)
    };
    let mut b = button(text(label).size(ts::SMALL).color(color))
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
