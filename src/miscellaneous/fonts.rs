//! The two typefaces Clew is set in, embedded so it reads the same on every
//! system: Inter for the interface, JetBrains Mono for code. Both are under
//! the SIL Open Font License (`assets/licenses/FONTS.md`).

use std::borrow::Cow;

use iced::Font;

/// The interface's typeface.
pub const UI: Font = Font::with_name("Inter");
/// The code's typeface.
pub const CODE: Font = Font::with_name("JetBrains Mono");

/// The interface's typeface, medium: the design's weight for what is set
/// a little apart — tabs, tools, headings over a group, keys, chips, the
/// item that is current.
pub const UI_MEDIUM: Font = Font {
    weight: iced::font::Weight::Medium,
    ..UI
};
/// The interface's typeface, semibold: titles and the names that lead a row.
pub const UI_SEMIBOLD: Font = Font {
    weight: iced::font::Weight::Semibold,
    ..UI
};
/// The code face, semibold: a function where it is defined, a note's name.
pub const CODE_SEMIBOLD: Font = Font {
    weight: iced::font::Weight::Semibold,
    ..CODE
};
/// The code face, bold: a symbol's name heading what is said about it.
pub const CODE_BOLD: Font = Font {
    weight: iced::font::Weight::Bold,
    ..CODE
};

/// `font` as a browser sets it in a control — a button, an input: in the
/// face's own spacing, where the design's tracking does not reach. Asked
/// for by a stretch the faces do not come in, which the vendored shaper
/// reads as that (`vendor/README.md`).
pub const fn in_control(font: Font) -> Font {
    Font {
        stretch: iced::font::Stretch::SemiExpanded,
        ..font
    }
}

/// `font` tracked as the design's titles are (`letter-spacing: -0.01em`),
/// by a stretch the vendored shaper reads as that ([`in_control`]).
pub const fn titled(font: Font) -> Font {
    Font {
        stretch: iced::font::Stretch::SemiCondensed,
        ..font
    }
}

/// Every embedded face: Inter regular, medium and semibold; JetBrains Mono
/// regular, semibold, bold and italic.
pub const ALL: [&[u8]; 7] = [
    include_bytes!("../../assets/fonts/Inter-Regular.otf"),
    include_bytes!("../../assets/fonts/Inter-Medium.otf"),
    include_bytes!("../../assets/fonts/Inter-SemiBold.otf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-SemiBold.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-Bold.ttf"),
    include_bytes!("../../assets/fonts/JetBrainsMono-Italic.ttf"),
];

/// Load the embedded faces into the renderer's font system, for a renderer
/// built without the app's builder (the tests' headless one). Idempotent.
#[cfg(test)]
pub fn load() {
    let mut system = iced::advanced::graphics::text::font_system()
        .write()
        .expect("the font system");
    for face in ALL {
        system.load_font(Cow::Borrowed(face));
    }
}

/// The faces as the app's builder takes them.
pub fn sources() -> impl Iterator<Item = Cow<'static, [u8]>> {
    ALL.into_iter().map(Cow::Borrowed)
}

#[cfg(test)]
mod tests {
    /// Italic words in Chinese are drawn with a face that has them: CJK
    /// faces have no italic, and asked only of italic faces the shaper
    /// found none — Ask's failure line in Chinese was all boxes.
    #[test]
    fn italic_chinese_finds_a_face_with_its_letters() {
        use iced::advanced::graphics::text::cosmic_text as ct;
        super::load();
        let mut system = iced::advanced::graphics::text::font_system()
            .write()
            .expect("the font system");
        let fonts = system.raw();
        let has_cjk = fonts
            .db()
            .faces()
            .any(|f| f.families.iter().any(|(name, _)| name.contains("CJK")));
        if !has_cjk {
            // Nothing to find on a machine with no CJK face.
            return;
        }
        let mut buffer = ct::Buffer::new(fonts, ct::Metrics::new(14.0, 18.0));
        for style in [ct::Style::Normal, ct::Style::Italic] {
            let attrs = ct::Attrs::new()
                .family(ct::Family::Name("Inter"))
                .style(style);
            buffer.set_text(fonts, "无法回答：AI", &attrs, ct::Shaping::Advanced, None);
            buffer.shape_until_scroll(fonts, false);
            let glyphs: Vec<u16> = buffer
                .layout_runs()
                .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
                .collect();
            assert!(
                !glyphs.is_empty() && glyphs.iter().all(|&g| g != 0),
                "{style:?}: a glyph with no face: {glyphs:?}"
            );
        }
    }
}
