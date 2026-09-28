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
//!
//! The rasterizing font is one system sans without fallback, so a label it
//! cannot fully cover (CJK, emoji, …) is not rasterized at all — the caller
//! draws it with canvas text instead, whose shaper falls back across system
//! fonts — rather than as a row of missing-glyph boxes. The cache is bounded
//! (least recently used entries go first), since labels × colors grows with
//! every project and theme the process sees.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ab_glyph::{Font, FontVec, ScaleFont};
use iced::widget::image::Handle;

/// Logical label font size (matches the old `fill_text` size).
pub(crate) const SIZE: f32 = 11.0;
/// Supersample factor: rasterize at 2× so the texture maps 1:1 on a 2× Retina
/// display and downsamples cleanly on 1×.
const SS: f32 = 2.0;
/// The most textures kept at once.
const CAPACITY: usize = 2048;

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

/// A least-recently-used map with a fixed capacity: inserting past it evicts
/// the oldest quarter at once, so eviction's sort is paid rarely.
pub(crate) struct Lru<K, V> {
    map: HashMap<K, (V, u64)>,
    clock: u64,
    capacity: usize,
}

impl<K: std::hash::Hash + Eq, V: Clone> Lru<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn get(&mut self, key: &K) -> Option<V> {
        self.clock += 1;
        let now = self.clock;
        self.map.get_mut(key).map(|(v, used)| {
            *used = now;
            v.clone()
        })
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.map.len() >= self.capacity && !self.map.contains_key(&key) {
            let mut ages: Vec<u64> = self.map.values().map(|(_, used)| *used).collect();
            ages.sort_unstable();
            // Drop the least recently used quarter (at least one entry).
            let cut = ages[(self.capacity / 4).max(1).min(ages.len()) - 1];
            self.map.retain(|_, (_, used)| *used > cut);
        }
        self.clock += 1;
        self.map.insert(key, (value, self.clock));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// Rasterized labels by (text, quantized color); `None` records a label the
/// font cannot draw, so it is not retried every frame.
type LabelCache = Lru<(String, u32), Option<LabelTexture>>;

static CACHE: LazyLock<Mutex<LabelCache>> = LazyLock::new(|| Mutex::new(Lru::new(CAPACITY)));

/// The label's texture in `color`, rasterized once and cached. `None` when it
/// cannot be drawn this way (no font, or glyphs the font lacks): the caller
/// then falls back to canvas text.
pub(crate) fn label_texture(label: &str, color: iced::Color) -> Option<LabelTexture> {
    let key = (label.to_string(), color_key(color));
    if let Ok(mut cache) = CACHE.lock()
        && let Some(hit) = cache.get(&key)
    {
        return hit;
    }
    let tex = rasterize(label, color);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(key, tex.clone());
    }
    tex
}

/// The width, in logical points, the label will be drawn at: the width of its
/// texture when [`label_texture`] can rasterize it, else an estimate for the
/// canvas-text fallback. Label decluttering and edge flipping use this, so
/// they agree with what is drawn.
pub(crate) fn label_width(label: &str) -> f32 {
    match LABEL_FONT
        .as_ref()
        .and_then(|font| texture_width_px(font, label))
    {
        Some(w_px) => w_px as f32 / SS,
        None => fallback_width(label),
    }
}

/// Estimated width of `label` as canvas text at [`SIZE`]: ~0.55 em per column,
/// wide glyphs (CJK, emoji) two columns.
pub(crate) fn fallback_width(label: &str) -> f32 {
    use unicode_width::UnicodeWidthChar;
    let cols: usize = label.chars().map(|c| c.width().unwrap_or(0)).sum();
    cols as f32 * SIZE * 0.55 + 2.0
}

/// Quantised RGB key for the cache (labels are re-rasterized when the theme
/// changes their colour).
fn color_key(c: iced::Color) -> u32 {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    (q(c.r) << 16) | (q(c.g) << 8) | q(c.b)
}

/// Whether `font` has a glyph for every visible character of `label`.
fn covers(font: &FontVec, label: &str) -> bool {
    label
        .chars()
        .all(|c| c.is_whitespace() || font.glyph_id(c).0 != 0)
}

/// Pixel width of the supersampled texture for `label`, or `None` when the
/// font cannot draw it.
fn texture_width_px(font: &FontVec, label: &str) -> Option<usize> {
    if !covers(font, label) {
        return None;
    }
    let scaled = font.as_scaled(SIZE * SS);
    let mut pen_x = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in label.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = prev {
            pen_x += scaled.kern(p, id);
        }
        pen_x += scaled.h_advance(id);
        prev = Some(id);
    }
    // A little right padding so a glyph that overhangs its advance isn't clipped.
    Some((pen_x.ceil() as usize + 2).max(1))
}

fn rasterize(label: &str, color: iced::Color) -> Option<LabelTexture> {
    let font = LABEL_FONT.as_ref()?;
    let w_px = texture_width_px(font, label)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_never_grows_past_its_capacity_and_keeps_recent_entries() {
        let mut lru: Lru<u32, u32> = Lru::new(8);
        for k in 0..8 {
            lru.insert(k, k);
        }
        // Touch the oldest so it counts as recent.
        assert_eq!(lru.get(&0), Some(0));
        for k in 8..40 {
            lru.insert(k, k);
            assert!(lru.len() <= 8, "grew to {}", lru.len());
        }
        // The newest entry is always there; the untouched early ones are gone.
        assert_eq!(lru.get(&39), Some(39));
        assert_eq!(lru.get(&1), None);
    }

    #[test]
    fn re_inserting_a_present_key_does_not_evict() {
        let mut lru: Lru<u32, u32> = Lru::new(2);
        lru.insert(1, 1);
        lru.insert(2, 2);
        lru.insert(2, 20);
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get(&1), Some(1));
        assert_eq!(lru.get(&2), Some(20));
    }

    /// A label the rasterizing font cannot fully draw is left to canvas text
    /// (which falls back across system fonts) instead of becoming a texture
    /// of missing-glyph boxes — and its width is estimated for that path.
    #[test]
    fn labels_the_font_cannot_cover_fall_back_to_canvas_text() {
        let Some(font) = LABEL_FONT.as_ref() else {
            // No system font on this machine: everything already falls back.
            assert!(label_texture("lib.rs", iced::Color::WHITE).is_none());
            return;
        };
        assert!(covers(font, "src/lib.rs"));
        assert!(label_texture("src/lib.rs", iced::Color::WHITE).is_some());
        let cjk = "数据库.rs";
        if !covers(font, cjk) {
            assert!(label_texture(cjk, iced::Color::WHITE).is_none());
            assert_eq!(label_width(cjk), fallback_width(cjk));
        }
    }

    /// Decluttering sizes a label by `label_width`; it must be the width the
    /// label's INK spans — measured on the rasterized pixels, not by the
    /// arithmetic `label_width` and the texture share: nothing drawn is cut
    /// off at the texture's right edge, and the texture is no wider than its
    /// ink plus the padding.
    #[test]
    fn label_width_matches_the_drawn_texture() {
        let label = "graph_labels.rs";
        let tex = label_texture(label, iced::Color::BLACK).expect("macOS ships the label font");
        let Handle::Rgba {
            width,
            height,
            pixels,
            ..
        } = &tex.handle
        else {
            panic!("a label texture is raw RGBA");
        };
        let (w, h) = (*width as usize, *height as usize);
        let inked: Vec<usize> = (0..w)
            .filter(|&x| (0..h).any(|y| pixels[(y * w + x) * 4 + 3] > 0))
            .collect();
        let (first, last) = (inked[0], *inked.last().expect("the label has ink"));
        assert!(first <= 4, "the ink starts {first}px in");
        assert!(last + 1 < w, "a glyph is cut off at the texture's edge");
        assert!(
            w - (last + 1) <= 2 * (SS as usize) + 2,
            "a {w}px texture for ink ending at {last}px"
        );
        assert_eq!(label_width(label), w as f32 / SS);
        // Wide glyphs count double in the fallback estimate.
        assert!(fallback_width("数据") > fallback_width("ab"));
    }
}
