//! Smooth graph labels.
//!
//! The 3D map's node labels used to be drawn with the canvas's `fill_text`,
//! which snaps text to the pixel grid. During the slow idle spin a label drifts
//! a fraction of a pixel per frame, so it sits on one pixel then snaps to the
//! next — a visible shake (the node circles, being vector shapes, don't snap and
//! stay smooth). Here each label is rasterized once to a small RGBA texture and
//! cached; the caller draws it with `draw_image` (`snap: false`, linear filter),
//! so the GPU samples it at the exact sub-pixel position and it glides smoothly,
//! just like the circles.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ab_glyph::{Font, FontVec, ScaleFont};
use iced::widget::image::Handle;

/// Logical label font size (matches the old `fill_text` size).
const SIZE: f32 = 11.0;
/// Supersample factor: rasterize at 2× so the texture maps 1:1 on a 2× Retina
/// display and downsamples cleanly on 1×.
const SS: f32 = 2.0;

/// The font labels are rasterized with: a macOS system sans, read at run time
/// (not redistributed). `None` if none is found, in which case the caller falls
/// back to plain canvas text.
static LABEL_FONT: LazyLock<Option<FontVec>> = LazyLock::new(|| {
    // The UI font first, then classic fallbacks.
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
    ];
    CANDIDATES.iter().find_map(|path| {
        let bytes = std::fs::read(path).ok()?;
        FontVec::try_from_vec_and_index(bytes, 0).ok()
    })
});

/// A rasterized label ready to draw: its image handle plus its size in *logical*
/// points (the texture itself is supersampled).
#[derive(Clone)]
pub(crate) struct LabelTexture {
    pub handle: Handle,
    pub width: f32,
    pub height: f32,
}

type Cache = HashMap<(String, u32), Option<LabelTexture>>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The label's texture in `color`, rasterized once and cached. `None` when no
/// font is available (caller falls back to text).
pub(crate) fn label_texture(label: &str, color: iced::Color) -> Option<LabelTexture> {
    let key = (label.to_string(), color_key(color));
    if let Ok(cache) = CACHE.lock()
        && let Some(hit) = cache.get(&key)
    {
        return hit.clone();
    }
    let tex = rasterize(label, color);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(key, tex.clone());
    }
    tex
}

/// Quantised RGB key for the cache (labels are re-rasterized when the theme
/// changes their colour).
fn color_key(c: iced::Color) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(c.r) << 16) | (q(c.g) << 8) | q(c.b)
}

fn rasterize(label: &str, color: iced::Color) -> Option<LabelTexture> {
    let font = LABEL_FONT.as_ref()?;
    let scaled = font.as_scaled(SIZE * SS);
    let ascent = scaled.ascent();
    let line_h = (ascent - scaled.descent()).ceil().max(1.0); // descent is negative

    // Lay glyphs out left→right along the baseline at y = ascent.
    let mut outlines = Vec::new();
    let mut pen_x = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in label.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            pen_x += scaled.kern(p, id);
        }
        let mut glyph = scaled.scaled_glyph(ch);
        glyph.position = ab_glyph::point(pen_x, ascent);
        if let Some(outlined) = scaled.outline_glyph(glyph) {
            outlines.push(outlined);
        }
        pen_x += scaled.h_advance(id);
        prev = Some(id);
    }
    // A little right padding so a glyph that overhangs its advance isn't clipped.
    let w_px = (pen_x.ceil() as usize + 2).max(1);
    let h_px = (line_h as usize).max(1);

    // Straight RGBA: the baked colour with per-pixel coverage as alpha.
    let mut buf = vec![0u8; w_px * h_px * 4];
    let rgb = [
        (color.r.clamp(0.0, 1.0) * 255.0) as u8,
        (color.g.clamp(0.0, 1.0) * 255.0) as u8,
        (color.b.clamp(0.0, 1.0) * 255.0) as u8,
    ];
    for outlined in &outlines {
        let bounds = outlined.px_bounds();
        let (ox, oy) = (bounds.min.x as i32, bounds.min.y as i32);
        outlined.draw(|gx, gy, cov| {
            let (px, py) = (ox + gx as i32, oy + gy as i32);
            if px < 0 || py < 0 || px >= w_px as i32 || py >= h_px as i32 {
                return;
            }
            let idx = (py as usize * w_px + px as usize) * 4;
            let a = (cov.clamp(0.0, 1.0) * 255.0) as u8;
            if a > buf[idx + 3] {
                buf[idx] = rgb[0];
                buf[idx + 1] = rgb[1];
                buf[idx + 2] = rgb[2];
                buf[idx + 3] = a;
            }
        });
    }
    Some(LabelTexture {
        handle: Handle::from_rgba(w_px as u32, h_px as u32, buf),
        width: w_px as f32 / SS,
        height: h_px as f32 / SS,
    })
}
