#[cfg(not(feature = "std"))]
// clew patch: `truncf` is no longer used (`LayoutGlyph::physical` rounds).
pub use libm::{floorf, roundf};

#[cfg(feature = "std")]
#[inline]
pub fn floorf(x: f32) -> f32 {
    x.floor()
}

#[cfg(feature = "std")]
#[inline]
pub fn roundf(x: f32) -> f32 {
    x.round()
}

