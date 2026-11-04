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
pub(crate) const STATUSBAR_H: f32 = 30.0;

/// One quiet item of the status bar.
fn status_text<'a>(s: impl iced::widget::text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(s).size(ts::SMALL).color(theme::fg_muted())
}

/// The status bar: one quiet line along the window's foot. On the left,
/// where the code is read from and what Clew is doing; on the right, the
/// open file — its language and language server, its problems, the caret,
/// and which platform its `#[cfg]`-style branches are read for.
pub(crate) fn statusbar(app: &App) -> Element<'_, Message> {
    // Where code is read from — the one place local vs remote shows. Click
    // it to manage connections / switch hosts.
    let remote = app.connection.is_remote();
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
                text(app.connection.label())
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
        container(
            text("Connect to a remote host")
                .size(ts::SMALL)
                .color(theme::fg()),
        )
        .padding([3, 7])
        .style(theme::modal_panel),
        tooltip::Position::Top,
    );

    let mut bar = row![conn_indicator, status_text(&app.status)]
        .spacing(14)
        .align_y(iced::Center);
    // A prominent, always-visible progress chip while "Explain All" runs — the
    // pass is slow, so show how far along it is (the status text alone is easy
    // to miss / read as stuck). It is the one place the pass's progress shows:
    // the refresh chip stays out of the way meanwhile (see `refresh_chip`).
    if app.proj.explain.running {
        let label = match app.proj.explain.progress {
            Some((done, total)) if total > 0 => format!("Explaining {done}/{total}"),
            _ => "Explaining…".to_string(),
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
            chip = chip.push(
                progress_bar(0.0..=total as f32, done as f32)
                    .length(90.0)
                    .girth(4.0)
                    .style(theme::progress),
            );
        }
        // Failures never hide behind the counter: a running tally in warn red.
        if app.proj.explain.failed > 0 {
            chip = chip.push(
                text(format!("· {} failed", app.proj.explain.failed))
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
            .map(|v| format!("  ·  {} lines", v.lines.len()))
            .unwrap_or_default();
        bar = bar.push(status_text(format!(
            "Time Travel  ·  {short}  ·  {}/{}{}{}",
            tt.idx + 1,
            tt.commits.len(),
            scope,
            lines
        )));
    } else if let Some(v) = app.active_viewer() {
        let lang = v
            .lang_key
            .and_then(crate::highlight::lang_name)
            .unwrap_or("Plain text");
        // Language-server status for this file's language, when relevant —
        // from the snapshot this update took (so a state clew cannot read
        // shows as a problem, not as "ready").
        let lsp = v
            .lang_key
            .and_then(|k| app.lsp_status(k))
            .map(|status| format!("  ·  LSP {status}"))
            .unwrap_or_default();
        bar = bar.push(status_text(format!(
            "{lang}  ·  {} lines{lsp}",
            v.lines.len()
        )));
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
            bar = bar.push(status_text(format!("Line {}, Column {}", l + 1, c + 1)));
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
        let picker = button(
            row![
                text("Reading for").size(ts::SMALL).color(theme::fg_muted()),
                text(app.proj.reading_target.to_string())
                    .size(ts::SMALL)
                    .color(theme::fg()),
                glyph::icon(Glyph::ChevronDown, theme::dim(), 11.0),
            ]
            .spacing(5)
            .align_y(iced::Center),
        )
        .style(theme::icon_button(app.show_target_menu))
        .padding([2, 7])
        .on_press(Message::Window(WindowMsg::ToggleTargetMenu));
        bar = bar.push(picker);
    }

    container(bar.spacing(16).padding(Padding {
        top: 0.0,
        right: STRIP_W - 4.0,
        bottom: 0.0,
        left: 10.0,
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
        ("↻ Refreshing overview…".to_string(), theme::accent(), false)
    } else if app.proj.building_embeddings {
        ("↻ Refreshing index…".to_string(), theme::accent(), false)
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
        (format!("↻ Update queued · {secs}s"), theme::accent(), true)
    } else {
        ("↻ Up to date".to_string(), theme::dim(), true)
    };
    let mut b = button(text(label).size(ts::SMALL).color(color))
        .style(theme::icon_button(false))
        .padding([2, 8]);
    if enabled {
        b = b.on_press(Message::Explain(ExplainMsg::RefreshAll));
    }
    Some(b.into())
}
