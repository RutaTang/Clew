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
        // Notebook outline entries: code cells and markdown headings.
        "cell" => "cell",
        "section" => "§",
        _ => "sym",
    }
}

pub(crate) fn kind_color(kind: &str) -> iced::Color {
    // Echoes the syntax token colors, so the dot follows the light/dark theme.
    let c = |dark: u32, light: u32| theme::rgb(if theme::is_light() { light } else { dark });
    match kind {
        "function" | "method" | "macro" => c(0x61afef, 0x4078f2),
        "class" | "struct" | "enum" | "union" | "trait" | "interface" | "type" => {
            c(0xe5c07b, 0xc18401)
        }
        "module" | "implementation" => c(0xc678dd, 0xa626a4),
        "constant" => c(0xd19a66, 0x986801),
        _ => theme::dim(),
    }
}

// ---------------------------------------------------------------- status bar

pub(crate) fn statusbar(app: &App) -> Element<'_, Message> {
    // In time travel, report the revision being viewed — not the live document's
    // stats (its line count / a caret line that may not exist in this revision).
    let right = if let Some(tt) = &app.time_travel {
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
        format!(
            "Time Travel  ·  {short}  ·  {}/{}{}{}",
            tt.idx + 1,
            tt.commits.len(),
            scope,
            lines
        )
    } else {
        match app.active_viewer() {
            Some(v) => {
                let lang = v
                    .lang_key
                    .and_then(crate::highlight::lang_name)
                    .unwrap_or("Plain text");
                // 1-based line/column of the last click, when there is one.
                let pos = v
                    .caret
                    .map(|(l, c)| format!("Ln {}, Col {}  ·  ", l + 1, c + 1))
                    .unwrap_or_default();
                // Language-server status for this file's language, when relevant.
                let lsp = v
                    .lang_key
                    .and_then(|k| app.lsp.get(k))
                    .map(|slot| format!("  ·  LSP {}", slot.label()))
                    .unwrap_or_default();
                // Diagnostic counts for this file.
                let diags = v
                    .lang_key
                    .and_then(|k| match app.lsp.get(k) {
                        Some(crate::LspSlot::Ready(c)) => Some(c.diagnostics(&v.abs)),
                        _ => None,
                    })
                    .map(|ds| {
                        let errs = ds.iter().filter(|d| d.severity == 1).count();
                        let warns = ds.iter().filter(|d| d.severity == 2).count();
                        if errs + warns == 0 {
                            String::new()
                        } else {
                            format!("  ·  ✘ {errs}  ⚠ {warns}")
                        }
                    })
                    .unwrap_or_default();
                format!(
                    "{}{}  ·  {} lines{}{}",
                    pos,
                    lang,
                    v.lines.len(),
                    diags,
                    lsp
                )
            }
            None => String::new(),
        }
    };

    // Where code is read from — the one place local vs remote shows. Click it to
    // manage connections / switch hosts.
    let (conn_glyph, conn_color) = if app.connection.is_remote() {
        (Glyph::Remote, theme::accent())
    } else {
        (Glyph::Circle, theme::dim())
    };
    let conn_indicator = tooltip(
        button(
            row![
                glyph::icon(conn_glyph, conn_color, 12.0),
                text(app.connection.label())
                    .size(11)
                    .color(if app.connection.is_remote() {
                        theme::accent()
                    } else {
                        theme::fg_muted()
                    }),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press(Message::OpenConnect),
        container(text("Connect to a remote host").size(11).color(theme::fg()))
            .padding([3, 7])
            .style(theme::modal_panel),
        tooltip::Position::Top,
    );

    let mut bar = row![conn_indicator, text(&app.status).size(11)]
        .spacing(12)
        .align_y(iced::Center);
    // A prominent, always-visible progress chip while "Explain All" runs — the
    // pass is slow, so show how far along it is (the status text alone is easy to
    // miss / read as stuck).
    if app.explain.running {
        let label = match app.explain.progress {
            Some((done, total)) if total > 0 => format!("Explaining {done}/{total}"),
            _ => "Explaining…".to_string(),
        };
        let mut chip = row![
            glyph::icon(Glyph::Sparkle, theme::accent(), 11.0),
            text(label).size(11).color(theme::accent()),
        ]
        .spacing(5)
        .align_y(iced::Center);
        // A short determinate bar once the total is known, so progress reads at a
        // glance instead of by parsing the counter.
        if let Some((done, total)) = app.explain.progress
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
        if app.explain.failed > 0 {
            chip = chip.push(
                text(format!("· {} failed", app.explain.failed))
                    .size(11)
                    .color(theme::warn()),
            );
        }
        // A cancel control right on the always-visible chip, so a long pass can
        // be stopped without hunting through menus.
        chip = chip.push(
            button(glyph::icon(Glyph::Close, theme::dim(), 11.0))
                .style(theme::toolbar_button)
                .padding([1, 4])
                .on_press(Message::CancelExplain),
        );
        bar = bar.push(chip);
    }
    bar = bar.push(space().width(Fill));
    if let Some(chip) = refresh_chip(app) {
        bar = bar.push(chip);
    }
    bar = bar.push(text(right).size(11));
    // For Rust files, a small target control that drives the `#[cfg]` dimming
    // (read another platform's branches as the live ones). A plain button + our
    // own dropdown, so the label and chevron sit tight together — placed last so
    // the popup anchored to the bottom-right lines up under it.
    if app.active_viewer().and_then(|v| v.lang_key) == Some("rust") {
        let picker = button(
            row![
                text(app.reading_target.to_string())
                    .size(11)
                    .color(theme::fg_muted()),
                glyph::icon(Glyph::ChevronDown, theme::dim(), 12.0),
            ]
            .spacing(4)
            .align_y(iced::Center),
        )
        .style(theme::toolbar_button)
        .padding([1, 6])
        .on_press(Message::ToggleTargetMenu);
        bar = bar.push(picker);
    }

    container(bar.padding([3, 10]))
        .width(Fill)
        .style(theme::statusbar)
        .into()
}

/// A freshness indicator for the auto-refreshed understanding: shows whether a
/// refresh is running / queued, and force-refreshes on click (bypassing the 30s
/// auto cooldown). Hidden until there's something to keep fresh (an explanation
/// set exists) and an LLM key is configured.
pub(crate) fn refresh_chip(app: &App) -> Option<Element<'_, Message>> {
    if !app.llm_available || app.explain.cache.is_empty() {
        return None;
    }
    // (label, colour, clickable). A running pass is shown but not clickable.
    let (label, color, enabled) = if app.explain.running {
        let l = match app.explain.progress {
            Some((done, total)) if total > 0 => format!("↻ Refreshing {done}/{total}…"),
            _ => "↻ Refreshing…".to_string(),
        };
        (l, theme::accent(), false)
    } else if app.overview.generating {
        ("↻ Refreshing overview…".to_string(), theme::accent(), false)
    } else if app.building_embeddings {
        ("↻ Refreshing index…".to_string(), theme::accent(), false)
    } else if app.refresh_pending {
        // Seconds left before the auto pass fires (click to skip the wait).
        let secs = app
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
    let mut b = button(text(label).size(11).color(color))
        .style(theme::toolbar_button)
        .padding([1, 8]);
    if enabled {
        b = b.on_press(Message::RefreshAll);
    }
    Some(b.into())
}

// ------------------------------------------------------ breakpoint condition
