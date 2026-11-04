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
