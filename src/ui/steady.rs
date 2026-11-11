//! Text that changes while it is watched — a count, a figure of progress, a
//! timer, a word for what is going on — set in a slot as wide as the widest
//! it will read, so nothing beside it moves as it changes. Figures are of
//! one width already (the vendored shaper's `tnum`); what is left is a
//! figure gaining a digit and a word giving way to a longer one.

use std::cell::RefCell;
use std::collections::HashMap;

use iced::advanced::text as core_text;
use iced::advanced::text::paragraph::Plain;
use iced::widget::{container, text};
use iced::{Element, Font, Size};

thread_local! {
    /// Widths measured, by text, size and font: a status bar redrawn ten
    /// times a second measures the same few strings.
    static MEASURED: RefCell<HashMap<(String, u32, Font), f32>> = RefCell::new(HashMap::new());
}

thread_local! {
    /// Texts cut to fit, by text, size, font and room: a list redrawn each
    /// frame cuts the same few paths.
    static ELIDED: RefCell<HashMap<(String, u32, Font, u32), String>> = RefCell::new(HashMap::new());
}

/// `content` cut from its start to fit `room` wide at `size` in `font`: an
/// ellipsis, then as much of its end as fits — from a `/` where one does, so
/// a path keeps its last folders whole. Whole when it fits; a bare ellipsis
/// when nothing of it does.
pub(crate) fn elide_start(content: &str, size: f32, font: Font, room: f32) -> String {
    if width_of(content, size, font) <= room {
        return content.to_string();
    }
    let key = (content.to_string(), size.to_bits(), font, room.to_bits());
    if let Some(cut) = ELIDED.with(|m| m.borrow().get(&key).cloned()) {
        return cut;
    }
    let fits = |at: usize| {
        let cut = format!("…{}", &content[at..]);
        (width_of(&cut, size, font) <= room).then_some(cut)
    };
    let at_separator = content
        .char_indices()
        .filter(|&(_, c)| c == '/')
        .find_map(|(i, _)| fits(i));
    let cut = at_separator
        .or_else(|| content.char_indices().skip(1).find_map(|(i, _)| fits(i)))
        .unwrap_or_else(|| "…".to_string());
    ELIDED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MEASURED_CAP {
            m.clear();
        }
        m.insert(key, cut.clone());
    });
    cut
}

/// `content` cut at its end to fit `room` wide at `size` in `font`: as much
/// of its start as fits, then an ellipsis. Whole when it fits.
pub(crate) fn elide_end(content: &str, size: f32, font: Font, room: f32) -> String {
    if width_of(content, size, font) <= room {
        return content.to_string();
    }
    let mut cut: Vec<usize> = content.char_indices().map(|(i, _)| i).collect();
    cut.reverse();
    cut.into_iter()
        .map(|at| format!("{}…", content[..at].trim_end()))
        .find(|s| width_of(s, size, font) <= room)
        .unwrap_or_else(|| "…".to_string())
}

/// How many widths [`width_of`] keeps before it forgets them all: more
/// than a search's results (2,000), each measured to be cut — at 512 the
/// cache was wiped several times over within one frame.
const MEASURED_CAP: usize = 8192;

thread_local! {
    /// Texts [`cut`] to fit, by text, size, font and room: a list of
    /// results redrawn each frame cut each of its rows again, a search of
    /// the measuring each.
    static CUT: RefCell<HashMap<(String, u32, Font, u32), String>> = RefCell::new(HashMap::new());
}

/// The width `content` is set in at `size` in `font`, on one line, to the
/// whole pixel above.
pub(crate) fn width_of(content: &str, size: f32, font: Font) -> f32 {
    let key = (content.to_string(), size.to_bits(), font);
    if let Some(width) = MEASURED.with(|m| m.borrow().get(&key).copied()) {
        return width;
    }
    let paragraph = Plain::<iced::advanced::graphics::text::Paragraph>::new(core_text::Text {
        content: content.to_string(),
        bounds: Size::INFINITE,
        size: size.into(),
        line_height: core_text::LineHeight::default(),
        font,
        align_x: core_text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: core_text::Shaping::Advanced,
        wrapping: core_text::Wrapping::None,
    });
    let width = paragraph.min_bounds().width.ceil();
    MEASURED.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MEASURED_CAP {
            m.clear();
        }
        m.insert(key, width);
    });
    width
}

/// The widest of `contents` at `size` in `font`.
pub(crate) fn widest(contents: &[&str], size: f32, font: Font) -> f32 {
    contents
        .iter()
        .map(|c| width_of(c, size, font))
        .fold(0.0, f32::max)
}

/// `shown`, set at `size` in the interface's font, in a slot as wide as the
/// widest of `widest_of` — what it will read at its widest (a count at its
/// total, the longest of the words it switches between) — and never
/// narrower than itself.
pub(crate) fn slot<'a, Message: 'a>(
    shown: text::Text<'a>,
    shown_content: &str,
    widest_of: &[&str],
    size: f32,
) -> Element<'a, Message> {
    let font = crate::fonts::UI;
    let width = widest(widest_of, size, font).max(width_of(shown_content, size, font));
    container(shown.wrapping(text::Wrapping::None))
        .width(width)
        .into()
}

/// `content` as it fits in `room` at `size` in `font`: whole, or cut short
/// with an ellipsis where it would run past.
pub(crate) fn cut(content: &str, room: f32, size: f32, font: Font) -> String {
    let key = (content.to_string(), size.to_bits(), font, room.to_bits());
    if let Some(cut) = CUT.with(|m| m.borrow().get(&key).cloned()) {
        return cut;
    }
    let cut = cut_measured(content, room, size, font);
    CUT.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() >= MEASURED_CAP {
            m.clear();
        }
        m.insert(key, cut.clone());
    });
    cut
}

/// [`cut`], measured.
fn cut_measured(content: &str, room: f32, size: f32, font: Font) -> String {
    if width_of(content, size, font) <= room {
        return content.to_string();
    }
    let chars: Vec<char> = content.chars().collect();
    let with = |n: usize| {
        let mut cut: String = chars[..n].iter().collect::<String>().trim_end().to_string();
        cut.push('…');
        cut
    };
    // The most characters that, with the ellipsis, still fit.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width_of(&with(mid), size, font) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    with(lo)
}

/// `n` with all its digits made the widest a figure is: what a count reads
/// at its widest once it has `n`'s digits. With figures of one width every
/// digit is as wide as `0`.
pub(crate) fn widest_figure(n: &str) -> String {
    n.chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Figures are of one width: a count reads as wide whatever its digits,
    /// and only a digit more widens it.
    #[test]
    fn figures_are_of_one_width() {
        let ui = crate::fonts::UI;
        crate::fonts::load();
        let one = width_of("Explaining 111/5289", 12.0, ui);
        let eight = width_of("Explaining 888/5289", 12.0, ui);
        assert!((one - eight).abs() < 0.5, "{one} vs {eight}");
        let more = width_of("Explaining 1111/5289", 12.0, ui);
        assert!(more > one, "{more} vs {one}");
    }

    /// A slot's width is what its widest reading would take.
    #[test]
    fn the_widest_is_the_widest() {
        let ui = crate::fonts::UI;
        crate::fonts::load();
        let short = width_of("Ready", 12.0, ui);
        let long = width_of("Starting…", 12.0, ui);
        assert_eq!(widest(&["Ready", "Starting…"], 12.0, ui), short.max(long));
        assert_eq!(widest_figure("1,234/5,289"), "0,000/0,000");
    }

    /// What does not fit is cut short with an ellipsis, and fits; what
    /// fits is left whole.
    #[test]
    fn what_does_not_fit_is_cut_short() {
        let ui = crate::fonts::UI;
        crate::fonts::load();
        let long = "rust-analyzer cargo check: hyper_rustls (lib)";
        let room = width_of("rust-analyzer cargo", 12.0, ui);
        let cut_short = cut(long, room, 12.0, ui);
        assert!(cut_short.ends_with('…'), "{cut_short}");
        assert!(width_of(&cut_short, 12.0, ui) <= room, "{cut_short}");
        assert!(long.starts_with(cut_short.trim_end_matches('…')));
        assert_eq!(cut("ready", room, 12.0, ui), "ready");
    }
}
