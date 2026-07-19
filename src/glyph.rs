//! Custom line icons for the chrome (toolbar, menus, nav, panel toggles), drawn
//! as authored SVG so they carry smooth curves and a consistent thin, rounded
//! stroke — one family, native to clew, in place of a third-party icon font.
//!
//! Each icon is original geometry on a 24-unit grid, `fill:none`, stroke width 2,
//! round caps and joins. The stroke color is injected per call (FG / DIM / …),
//! and the SVG is rendered by iced's `svg` widget (the same resvg backend used
//! for math/mermaid).

use iced::widget::svg;
use iced::{Color, Element};

/// Which icon to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    // Toolbar
    Overview,
    Stats,
    Ask,
    Debug,
    CallGraph,
    ImportGraph,
    Settings,
    // More menu
    Note,
    Info,
    Lightbulb,
    Minimap,
    Sparkle,
    Compass,
    Skim,
    Folder,
    Diff,
    TimeTravel,
    Servers,
    Shortcuts,
    // Chrome: nav + panel toggles
    ArrowLeft,
    ArrowRight,
    PanelLeft,
    PanelRight,
}

/// The inner SVG for a glyph — original geometry, 24×24, stroked (a few filled
/// dots set `fill`/`stroke` inline). Single-quoted attributes so the wrapper's
/// double-quoted `stroke` substitution stays clean.
fn body(g: Glyph) -> &'static str {
    use Glyph::*;
    match g {
        Overview => "<path d='M12 6.8 C9.8 5.8 6.3 5.6 4.3 6.1 V17.8 C6.3 17.3 9.8 17.5 12 18.5'/>\
             <path d='M12 6.8 C14.2 5.8 17.7 5.6 19.7 6.1 V17.8 C17.7 17.3 14.2 17.5 12 18.5'/>\
             <path d='M12 6.8 V18.5'/>",
        Stats => "<path d='M4.5 19 H19.5'/><path d='M8 19 V11'/><path d='M12 19 V5'/><path d='M16 19 V13'/>",
        Ask => "<path d='M6.5 5 H17.5 A2.5 2.5 0 0 1 20 7.5 V13.5 A2.5 2.5 0 0 1 17.5 16 H10 L6 19.5 V16 A2.5 2.5 0 0 1 3.5 13.5 V7.5 A2.5 2.5 0 0 1 6.5 5 Z'/>",
        Debug => "<path d='M8 10 C8 7.2 9.8 5.5 12 5.5 C14.2 5.5 16 7.2 16 10 V13.5 C16 16.3 14.2 18 12 18 C9.8 18 8 16.3 8 13.5 Z'/>\
             <path d='M12 6 V17.5'/><path d='M9.8 6 L8 4'/><path d='M14.2 6 L16 4'/>\
             <path d='M8 10.5 L4.5 9.5'/><path d='M8 14 L4.5 15.5'/><path d='M16 10.5 L19.5 9.5'/><path d='M16 14 L19.5 15.5'/>",
        CallGraph => "<path d='M12 7.5 L7 15'/><path d='M12 7.5 L17 15'/>\
             <circle cx='12' cy='5.8' r='2.2'/><circle cx='6' cy='16.5' r='2.2'/><circle cx='18' cy='16.5' r='2.2'/>",
        ImportGraph => "<rect x='4' y='4' width='7' height='7' rx='1.5'/><rect x='13' y='13' width='7' height='7' rx='1.5'/>\
             <path d='M11.2 11.2 L12.8 12.8'/>",
        Settings => "<path d='M4 8.5 H12.5'/><path d='M16.5 8.5 H20'/><circle cx='14.5' cy='8.5' r='2.3'/>\
             <path d='M4 15.5 H7.5'/><path d='M11.5 15.5 H20'/><circle cx='9.5' cy='15.5' r='2.3'/>",
        Note => "<rect x='5' y='3.5' width='14' height='17' rx='2'/>\
             <path d='M8.5 8 H15.5'/><path d='M8.5 11.5 H15.5'/><path d='M8.5 15 H13'/>",
        Info => "<circle cx='12' cy='12' r='8.5'/><path d='M12 11 V16'/>\
             <circle cx='12' cy='8.2' r='0.9' fill='STROKE' stroke='none'/>",
        Lightbulb => "<path d='M12 4 A5 5 0 0 1 15 13 C14.4 13.8 14 14.4 14 15.5 H10 C10 14.4 9.6 13.8 9 13 A5 5 0 0 1 12 4 Z'/>\
             <path d='M10 18 H14'/><path d='M10.7 20 H13.3'/>",
        Minimap => "<path d='M4 6.5 L9.5 4.5 L14.5 6.5 L20 4.5 V17.5 L14.5 19.5 L9.5 17.5 L4 19.5 Z'/>\
             <path d='M9.5 4.5 V17.5'/><path d='M14.5 6.5 V19.5'/>",
        Sparkle => "<path d='M12 4 L13.4 10.6 L20 12 L13.4 13.4 L12 20 L10.6 13.4 L4 12 L10.6 10.6 Z'/>",
        Compass => "<circle cx='12' cy='12' r='8.5'/><path d='M12 6.5 L13.6 12 L12 17.5 L10.4 12 Z'/>",
        Skim => "<path d='M4.5 8 L12 12 L19.5 8'/><path d='M4.5 13.5 L12 17.5 L19.5 13.5'/>",
        Folder => "<path d='M3.5 6.5 A1.5 1.5 0 0 1 5 5 H9 L11 7 H19 A1.5 1.5 0 0 1 20.5 8.5 V17 A1.5 1.5 0 0 1 19 18.5 H5 A1.5 1.5 0 0 1 3.5 17 Z'/>",
        Diff => "<rect x='3.5' y='4' width='17' height='16' rx='2'/><path d='M12 4 V20'/>\
             <path d='M5.5 12 H9.5'/><path d='M7.5 10 V14'/><path d='M14.5 12 H18.5'/>",
        TimeTravel => "<circle cx='12' cy='12' r='8.5'/><path d='M12 7.5 V12 L15 14'/>",
        Servers => "<rect x='3.5' y='4.5' width='17' height='6' rx='1.5'/>\
             <rect x='3.5' y='13.5' width='17' height='6' rx='1.5'/>\
             <circle cx='7' cy='7.5' r='0.85' fill='STROKE' stroke='none'/>\
             <circle cx='7' cy='16.5' r='0.85' fill='STROKE' stroke='none'/>",
        Shortcuts => "<rect x='2.5' y='6' width='19' height='12' rx='2'/>\
             <path d='M6 10 H6.01'/><path d='M9 10 H9.01'/><path d='M12 10 H12.01'/><path d='M15 10 H15.01'/><path d='M18 10 H18.01'/>\
             <path d='M8 14 H16'/>",
        ArrowLeft => "<path d='M11 5.5 L4.5 12 L11 18.5'/><path d='M5 12 H19.5'/>",
        ArrowRight => "<path d='M13 5.5 L19.5 12 L13 18.5'/><path d='M19 12 H4.5'/>",
        PanelLeft => "<rect x='3.5' y='4' width='17' height='16' rx='2.2'/><path d='M9.5 4 V20'/>",
        PanelRight => "<rect x='3.5' y='4' width='17' height='16' rx='2.2'/><path d='M14.5 4 V20'/>",
    }
}

/// `iced::Color` → `#rrggbb`.
fn hex(c: Color) -> String {
    let u = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", u(c.r), u(c.g), u(c.b))
}

/// A square icon widget: `glyph` stroked in `color` at `size` logical px.
pub fn icon<'a, M: 'a>(glyph: Glyph, color: Color, size: f32) -> Element<'a, M> {
    let stroke = hex(color);
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='{stroke}' \
         stroke-width='1.9' stroke-linecap='round' stroke-linejoin='round'>{}</svg>",
        body(glyph).replace("STROKE", &stroke),
    );
    svg(svg::Handle::from_memory(doc.into_bytes()))
        .width(size)
        .height(size)
        .into()
}
