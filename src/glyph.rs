//! Hand-drawn line icons for the toolbar and menus, drawn with iced's canvas so
//! they share the exact stroke "hand" as the window's traffic-light glyphs
//! (round caps, round joins, thin weight) rather than a third-party icon font.
//!
//! Every icon is authored on a 16-unit grid (y-down) as polylines and dots, then
//! scaled to the widget's box. One color, one stroke family — so the whole set
//! reads as a single family native to clew.

use iced::widget::canvas::{self, Frame, LineCap, LineJoin, Path, Stroke};
use iced::{Color, Element, Point, Rectangle, Renderer};

/// Stroke weight in grid units (≈1.35px at a 16px icon) — a hair above the
/// traffic-light weight since these are primary content, not hover-only.
const STROKE: f32 = 1.35;

/// Which icon to draw. Toolbar actions first, then the More-menu items.
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
}

/// One drawable primitive in grid units.
enum Prim {
    /// A polyline through these points; `true` closes it back to the start.
    Line(Vec<(f32, f32)>, bool),
    /// A filled dot: center x, y and radius.
    Dot(f32, f32, f32),
}

// -- geometry helpers (grid units, y-down; angles in degrees) ---------------

fn circle(cx: f32, cy: f32, r: f32) -> Vec<(f32, f32)> {
    (0..48)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / 48.0;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

fn arc(cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Vec<(f32, f32)> {
    let n = 16;
    (0..=n)
        .map(|i| {
            let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// A rounded-rectangle outline (clockwise, y-down) matching the design script.
fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Vec<(f32, f32)> {
    let mut p = Vec::new();
    p.extend(arc(x1 - r, y0 + r, r, -90.0, 0.0)); // top-right
    p.extend(arc(x1 - r, y1 - r, r, 0.0, 90.0)); // bottom-right
    p.extend(arc(x0 + r, y1 - r, r, 90.0, 180.0)); // bottom-left
    p.extend(arc(x0 + r, y0 + r, r, 180.0, 270.0)); // top-left
    p
}

fn line(pts: &[(f32, f32)]) -> Prim {
    Prim::Line(pts.to_vec(), false)
}

/// The primitives for one glyph, in grid units.
fn primitives(g: Glyph) -> Vec<Prim> {
    use Glyph::*;
    match g {
        // ---- toolbar (exact geometry from the approved design) ----
        Overview => vec![
            // Open book: two pages whose edges gently sag, plus a center spine.
            line(&[(8.0, 4.6), (5.3, 4.35), (2.7, 5.2), (2.7, 11.4), (5.3, 11.9), (8.0, 12.2)]),
            line(&[(8.0, 4.6), (10.7, 4.35), (13.3, 5.2), (13.3, 11.4), (10.7, 11.9), (8.0, 12.2)]),
            line(&[(8.0, 4.6), (8.0, 12.2)]),
        ],
        Stats => vec![
            line(&[(3.0, 12.7), (13.0, 12.7)]), // baseline
            line(&[(5.0, 12.3), (5.0, 9.6)]),
            line(&[(8.0, 12.3), (8.0, 5.4)]),
            line(&[(11.0, 12.3), (11.0, 7.8)]),
        ],
        Ask => vec![
            Prim::Line(rrect(2.8, 3.6, 13.2, 10.6, 2.4), true),
            line(&[(6.2, 10.4), (4.3, 13.0), (8.2, 10.4)]), // tail
        ],
        Debug => {
            let mut v = vec![
                Prim::Line(rrect(5.6, 5.2, 10.4, 12.4, 2.4), true),
                line(&[(8.0, 6.4), (8.0, 11.3)]),   // seam
                line(&[(6.7, 5.5), (5.3, 3.7)]),    // antenna L
                line(&[(9.3, 5.5), (10.7, 3.7)]),   // antenna R
            ];
            for (y, dy) in [(7.4, -1.0), (9.0, 0.0), (10.6, 1.0)] {
                v.push(line(&[(5.7, y), (3.7, y + dy)]));
                v.push(line(&[(10.3, y), (12.3, y + dy)]));
            }
            v
        }
        CallGraph => vec![
            line(&[(8.0, 5.5), (4.9, 10.5)]), // edges first (under nodes)
            line(&[(8.0, 5.5), (11.1, 10.5)]),
            Prim::Line(circle(8.0, 4.0, 1.7), true),
            Prim::Line(circle(4.6, 12.0, 1.7), true),
            Prim::Line(circle(11.4, 12.0, 1.7), true),
        ],
        ImportGraph => vec![
            Prim::Line(rrect(2.6, 2.8, 6.9, 7.1, 1.3), true),
            Prim::Line(rrect(9.1, 8.9, 13.4, 13.2, 1.3), true),
            line(&[(7.0, 7.2), (9.0, 8.8)]),   // arrow shaft
            line(&[(9.0, 8.8), (7.3, 8.55)]),  // barb
            line(&[(9.0, 8.8), (8.75, 7.1)]),  // barb
        ],
        Settings => vec![
            line(&[(3.0, 5.6), (13.0, 5.6)]), // slider tracks
            line(&[(3.0, 10.4), (13.0, 10.4)]),
            Prim::Line(circle(10.0, 5.6, 1.55), true), // knobs
            Prim::Line(circle(6.0, 10.4, 1.55), true),
        ],
        // ---- more menu (same vocabulary: circles + rounded rects) ----
        Note => vec![
            Prim::Line(rrect(3.5, 3.5, 12.5, 12.5, 1.8), true),
            line(&[(5.6, 7.0), (10.4, 7.0)]),
            line(&[(5.6, 9.4), (9.0, 9.4)]),
        ],
        Info => vec![
            Prim::Line(circle(8.0, 8.0, 4.3), true),
            Prim::Dot(8.0, 5.85, 0.55),
            line(&[(8.0, 7.5), (8.0, 10.5)]),
        ],
        Lightbulb => vec![
            Prim::Line(circle(8.0, 6.6, 3.0), true),
            line(&[(6.5, 10.2), (9.5, 10.2)]),
            line(&[(6.8, 11.6), (9.2, 11.6)]),
        ],
        Minimap => vec![
            Prim::Line(rrect(3.4, 4.0, 12.6, 12.0, 1.3), true),
            line(&[(6.5, 4.2), (6.5, 11.8)]), // folds
            line(&[(9.5, 4.2), (9.5, 11.8)]),
        ],
        Sparkle => vec![Prim::Line(
            // Four-point star (tips + inner points).
            vec![
                (8.0, 3.4),
                (9.0, 7.0),
                (12.6, 8.0),
                (9.0, 9.0),
                (8.0, 12.6),
                (7.0, 9.0),
                (3.4, 8.0),
                (7.0, 7.0),
            ],
            true,
        )],
        Compass => vec![
            Prim::Line(circle(8.0, 8.0, 4.3), true),
            Prim::Line(vec![(8.0, 5.4), (9.2, 8.0), (8.0, 10.6), (6.8, 8.0)], true), // needle
            Prim::Dot(8.0, 8.0, 0.5),
        ],
        Skim => vec![
            line(&[(5.0, 6.0), (8.0, 8.4), (11.0, 6.0)]), // two down-chevrons
            line(&[(5.0, 9.2), (8.0, 11.6), (11.0, 9.2)]),
        ],
        Folder => vec![Prim::Line(
            vec![
                (3.0, 5.2),
                (6.2, 5.2),
                (7.3, 6.7),
                (13.0, 6.7),
                (13.0, 12.2),
                (3.0, 12.2),
            ],
            true,
        )],
        Diff => vec![
            Prim::Line(rrect(3.0, 3.8, 13.0, 12.2, 1.6), true),
            line(&[(8.0, 3.9), (8.0, 12.1)]), // divider
            line(&[(5.5, 6.4), (5.5, 9.6)]),  // plus (left)
            line(&[(4.2, 8.0), (6.8, 8.0)]),
            line(&[(9.4, 8.0), (11.6, 8.0)]), // minus (right)
        ],
        TimeTravel => vec![
            Prim::Line(circle(8.0, 8.0, 4.3), true),
            line(&[(8.0, 8.0), (8.0, 5.2)]),  // hands
            line(&[(8.0, 8.0), (10.2, 9.0)]),
            Prim::Dot(8.0, 8.0, 0.5),
        ],
        Servers => vec![
            Prim::Line(rrect(3.2, 4.0, 12.8, 7.2, 1.0), true),
            Prim::Line(rrect(3.2, 8.8, 12.8, 12.0, 1.0), true),
            Prim::Dot(5.2, 5.6, 0.55),
            Prim::Dot(5.2, 10.4, 0.55),
        ],
        Shortcuts => vec![
            Prim::Line(rrect(2.4, 5.0, 13.6, 11.0, 1.4), true),
            Prim::Dot(4.7, 7.3, 0.5),
            Prim::Dot(6.9, 7.3, 0.5),
            Prim::Dot(9.1, 7.3, 0.5),
            Prim::Dot(11.3, 7.3, 0.5),
            line(&[(5.4, 9.3), (10.6, 9.3)]), // spacebar
        ],
    }
}

/// The canvas program that strokes one glyph in `color`.
pub struct GlyphProgram {
    pub glyph: Glyph,
    pub color: Color,
}

impl<M> canvas::Program<M> for GlyphProgram {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let s = bounds.width.min(bounds.height) / 16.0;
        let pen = || {
            Stroke::default()
                .with_width(STROKE * s)
                .with_color(self.color)
                .with_line_cap(LineCap::Round)
                .with_line_join(LineJoin::Round)
        };
        for prim in primitives(self.glyph) {
            match prim {
                Prim::Line(pts, closed) => {
                    let path = Path::new(|b| {
                        for (i, (x, y)) in pts.iter().enumerate() {
                            let pt = Point::new(x * s, y * s);
                            if i == 0 {
                                b.move_to(pt);
                            } else {
                                b.line_to(pt);
                            }
                        }
                        if closed {
                            b.close();
                        }
                    });
                    frame.stroke(&path, pen());
                }
                Prim::Dot(cx, cy, r) => {
                    frame.fill(&Path::circle(Point::new(cx * s, cy * s), r * s), self.color);
                }
            }
        }
        vec![frame.into_geometry()]
    }
}

/// A square canvas widget drawing `glyph` at `size` logical px in `color`.
pub fn icon<'a, M: 'a>(glyph: Glyph, color: Color, size: f32) -> Element<'a, M> {
    canvas::Canvas::new(GlyphProgram { glyph, color })
        .width(size)
        .height(size)
        .into()
}
