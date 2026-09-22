//! CPU rendering of the overlay: a pixel buffer, a few primitives, the coded border and the
//! calibration scene. No GPU and no font files; numbers are seven-segment glyphs.

use std::time::Instant;

use super::scene::{Quality, Scene};

pub const BLACK: u32 = 0x0000_0000;
pub const WHITE: u32 = 0x00FF_FFFF;
const GREY: u32 = 0x0050_5050;
const RED: u32 = 0x00FF_3030;
const GREEN: u32 = 0x0030_D030;
const AMBER: u32 = 0x00E0_A020;
const CYAN: u32 = 0x0040_E0E0;

/// Round a screen-space f64 to a pixel coordinate.
///
/// Everything drawn here is derived from the window size, so values are a few thousand at
/// most and the conversion cannot meaningfully truncate; `Canvas` clamps them again anyway.
#[allow(clippy::cast_possible_truncation)]
pub fn px(v: f64) -> i64 {
    v.round() as i64
}

/// Size and colour of the seven-segment digits.
#[derive(Clone, Copy, Debug)]
pub struct Glyph {
    pub w: i64,
    pub h: i64,
    pub thickness: i64,
    pub colour: u32,
}

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    pub w: i64,
    pub h: i64,
}

// Every coordinate is clamped to the buffer before it is used as an index, and the buffer is
// at most a screen in size, so the casts below cannot truncate or go negative.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
impl Canvas<'_> {
    pub fn fill(&mut self, c: u32) {
        self.px.fill(c);
    }

    pub fn rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: u32) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        // Entirely off-screen: the row slice below would be an inverted range.
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        for yy in y0..y1 {
            let row = (yy * self.w) as usize;
            self.px[row + x0 as usize..row + x1 as usize].fill(c);
        }
    }

    /// A hollow rectangle of the given thickness, drawn inward from the edges.
    pub fn frame(&mut self, x: i64, y: i64, w: i64, h: i64, t: i64, c: u32) {
        self.rect(x, y, w, t, c);
        self.rect(x, y + h - t, w, t, c);
        self.rect(x, y, t, h, c);
        self.rect(x + w - t, y, t, h, c);
    }

    pub fn disc(&mut self, cx: i64, cy: i64, r: i64, c: u32) {
        for dy in -r..=r {
            let dx = ((r * r - dy * dy) as f64).sqrt() as i64;
            self.rect(cx - dx, cy + dy, 2 * dx + 1, 1, c);
        }
    }

    pub fn ring(&mut self, cx: i64, cy: i64, r: i64, t: i64, c: u32) {
        let inner = (r - t).max(0);
        for dy in -r..=r {
            let outer_dx = ((r * r - dy * dy).max(0) as f64).sqrt() as i64;
            if dy.abs() <= inner {
                let inner_dx = ((inner * inner - dy * dy).max(0) as f64).sqrt() as i64;
                self.rect(cx - outer_dx, cy + dy, outer_dx - inner_dx + 1, 1, c);
                self.rect(cx + inner_dx, cy + dy, outer_dx - inner_dx + 1, 1, c);
            } else {
                self.rect(cx - outer_dx, cy + dy, 2 * outer_dx + 1, 1, c);
            }
        }
    }

    pub fn cross(&mut self, cx: i64, cy: i64, r: i64, t: i64, c: u32) {
        self.rect(cx - t / 2, cy - r, t, 2 * r, c);
        self.rect(cx - r, cy - t / 2, 2 * r, t, c);
    }

    pub fn line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, t: i64, c: u32) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        for i in 0..=steps {
            let x = x0 + (x1 - x0) * i / steps;
            let y = y0 + (y1 - y0) * i / steps;
            self.rect(x - t / 2, y - t / 2, t, t, c);
        }
    }

    /// One seven-segment digit. Avoids shipping a font for the only text that needs to be
    /// exact: target numbers.
    fn digit(&mut self, d: u8, x: i64, y: i64, s: Glyph) {
        let (w, h, t, c) = (s.w, s.h, s.thickness, s.colour);
        //      a
        //    f   b
        //      g
        //    e   c
        //      d
        const SEGS: [u8; 10] = [0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F];
        let s = SEGS[(d % 10) as usize];
        let mid = y + h / 2;
        let on = |bit: u8| s & (1 << bit) != 0;
        if on(0) {
            self.rect(x, y, w, t, c);
        }
        if on(1) {
            self.rect(x + w - t, y, t, h / 2, c);
        }
        if on(2) {
            self.rect(x + w - t, mid, t, h / 2, c);
        }
        if on(3) {
            self.rect(x, y + h - t, w, t, c);
        }
        if on(4) {
            self.rect(x, mid, t, h / 2, c);
        }
        if on(5) {
            self.rect(x, y, t, h / 2, c);
        }
        if on(6) {
            self.rect(x, mid - t / 2, w, t, c);
        }
    }

    /// A non-negative integer, centred on `cx`.
    pub fn number(&mut self, mut v: u32, cx: i64, y: i64, s: Glyph) {
        let mut digits = Vec::new();
        loop {
            digits.push((v % 10) as u8);
            v /= 10;
            if v == 0 {
                break;
            }
        }
        digits.reverse();
        let gap = s.w / 4;
        let n = i64::try_from(digits.len()).unwrap_or(1);
        let mut x = cx - (n * (s.w + gap) - gap) / 2;
        for d in digits {
            self.digit(d, x, y, s);
            x += s.w + gap;
        }
    }
}

/// Draw the tracked border: the white frame plus the tabs that make it self-locating (one
/// border thickness inward from the inner edge, spanning the percent range the code table
/// gives). Returns the thickness in pixels.
pub fn draw_border(c: &mut Canvas, w: u32, h: u32, border_frac: f64) -> i64 {
    use crate::vision::code::{tabs, Side};
    let t = px(f64::from(w.min(h)) * border_frac).max(2);
    let (wi, hi) = (i64::from(w), i64::from(h));
    c.frame(0, 0, wi, hi, t, WHITE);
    for side in Side::ALL {
        for tab in tabs(side) {
            let len = if side.is_horizontal() { w } else { h };
            let a = px(tab.start / 100.0 * f64::from(len));
            let b = px(tab.end / 100.0 * f64::from(len));
            match side {
                Side::Top => c.rect(a, t, b - a, t, WHITE),
                Side::Bottom => c.rect(a, hi - 2 * t, b - a, t, WHITE),
                Side::Left => c.rect(t, a, t, b - a, WHITE),
                Side::Right => c.rect(wi - 2 * t, a, t, b - a, WHITE),
            }
        }
    }
    t
}

/// An axis-aligned rectangle in window pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// The rectangles [`draw_border`] fills, for a window shape that shows only the border and
/// lets the game underneath show through everywhere else.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn border_rects(w: u32, h: u32, border_frac: f64) -> Vec<Rect> {
    use crate::vision::code::{tabs, Side};
    let t = px(f64::from(w.min(h)) * border_frac).max(2);
    let (wi, hi) = (i64::from(w), i64::from(h));
    let r = |x: i64, y: i64, rw: i64, rh: i64| Rect {
        x: x as i32,
        y: y as i32,
        w: rw.max(0) as u32,
        h: rh.max(0) as u32,
    };
    let mut out = vec![
        r(0, 0, wi, t),
        r(0, hi - t, wi, t),
        r(0, 0, t, hi),
        r(wi - t, 0, t, hi),
    ];
    for side in Side::ALL {
        for tab in tabs(side) {
            let len = if side.is_horizontal() { w } else { h };
            let a = px(tab.start / 100.0 * f64::from(len));
            let b = px(tab.end / 100.0 * f64::from(len));
            out.push(match side {
                Side::Top => r(a, t, b - a, t),
                Side::Bottom => r(a, hi - 2 * t, b - a, t),
                Side::Left => r(t, a, t, b - a),
                Side::Right => r(wi - 2 * t, a, t, b - a),
            });
        }
    }
    out
}

/// Render the whole scene into `px` (row-major `0x00RRGGBB`, `w * h` long).
pub fn render(buf: &mut [u32], w: u32, h: u32, scene: &Scene) {
    let mut c = Canvas {
        px: buf,
        w: i64::from(w),
        h: i64::from(h),
    };
    c.fill(BLACK);

    // The border the gun actually tracks. Everything else is smaller than it and inside.
    let t = draw_border(&mut c, w, h, scene.border_frac);

    let to_px = |p: [f64; 2]| -> (i64, i64) {
        (
            px(p[0] / 100.0 * f64::from(w)),
            px(p[1] / 100.0 * f64::from(h)),
        )
    };
    let unit = i64::from(w.min(h)) / 60;
    let flashing = scene.flash_until.is_some_and(|u| Instant::now() < u);

    for (i, tgt) in scene.targets.iter().enumerate() {
        let (x, y) = to_px(*tgt);
        let measured = scene.measured.get(i).copied().flatten();
        let is_current = scene.current == Some(i);
        let colour = if measured.is_some() {
            GREEN
        } else if is_current {
            if flashing {
                RED
            } else {
                WHITE
            }
        } else {
            GREY
        };
        c.cross(x, y, unit * 2, (unit / 4).max(2), colour);
        if is_current && !scene.done {
            // A steady amber ring marks the target to shoot; red when a shot could not
            // be measured. Colour, not motion, so it never fights the tracking light.
            c.ring(
                x,
                y,
                unit * 2,
                (unit / 3).max(2),
                if flashing { RED } else { AMBER },
            );
            let g = Glyph {
                w: unit * 2,
                h: unit * 3,
                thickness: (unit / 3).max(2),
                colour: AMBER,
            };
            // The number sits to the right of the target, where every grid position
            // has room; above it, the top row ran into the border.
            c.number(
                u32::try_from(i + 1).unwrap_or(0),
                x + unit * 6,
                y - unit * 3 / 2,
                g,
            );
        }
        if let Some(m) = measured {
            // Draw the error as a vector from where you aimed to what the driver read,
            // so the shape of the distortion is visible at a glance.
            let (mx, my) = to_px(m);
            c.line(x, y, mx, my, (unit / 4).max(2), GREEN);
            c.disc(mx, my, (unit / 2).max(2), GREEN);
        }
    }

    if let Some(aim) = scene.aim {
        let (x, y) = to_px(aim);
        let colour = match scene.quality {
            Quality::Good => CYAN,
            Quality::Clipped => AMBER,
            Quality::Lost => RED,
        };
        c.ring(x, y, unit, (unit / 3).max(2), colour);
    }

    // Where the camera is looking: its frame projected onto the screen.
    // A one-side solve knows nothing about the foreshortening across that side, so its
    // far corners are a guess and the outline would mislead; it is drawn only when at
    // least two edges pinned the solve.
    if let (Some(v), true) = (scene.solve.view, scene.solve.sides.count_ones() >= 2) {
        let p: Vec<(i64, i64)> = v.iter().map(|q| to_px(*q)).collect();
        let colour = if scene.solve.from_lines { GREY } else { RED };
        for i in 0..4 {
            let (a, b) = (p[i], p[(i + 1) % 4]);
            c.line(a.0, a.1, b.0, b.1, (unit / 6).max(1), colour);
        }
    }

    // Tracking quality, inside the border so it never interferes with detection.
    // Amber also for a one-side solve: usable near that edge, approximate elsewhere.
    let q = match scene.quality {
        Quality::Good if scene.solve.sides.count_ones() <= 1 => AMBER,
        Quality::Good => GREEN,
        Quality::Clipped => AMBER,
        Quality::Lost => RED,
    };
    // The status panel sits along the bottom edge a quarter of the way across, between
    // the bottom-left and bottom-centre targets of a grid.
    let x0 = i64::from(w) / 4;
    let y0 = i64::from(h) - t * 2 - unit * 4;
    c.rect(x0, y0, unit * 3, unit, q);
    // Which sides the solve had: a small frame with one bar per fitted side.
    let (fx, fy, fw, fh, bar) = (
        x0 + unit * 4,
        y0 - unit,
        unit * 4,
        unit * 3,
        (unit / 3).max(2),
    );
    for (i, (x, y, bw, bh)) in [
        (fx, fy, fw, bar),
        (fx + fw - bar, fy, bar, fh),
        (fx, fy + fh - bar, fw, bar),
        (fx, fy, bar, fh),
    ]
    .into_iter()
    .enumerate()
    {
        let on = scene.solve.sides & (1 << i) != 0;
        c.rect(x, y, bw, bh, if on { GREEN } else { GREY });
    }
    // Decoded tab count.
    let g = Glyph {
        w: unit,
        h: unit * 2,
        thickness: (unit / 4).max(2),
        colour: if scene.solve.tabs > 0 { CYAN } else { GREY },
    };
    c.number(u32::from(scene.solve.tabs), fx + fw + unit * 2, fy, g);

    if scene.done {
        let g = Glyph {
            w: unit * 2,
            h: unit * 3,
            thickness: (unit / 3).max(2),
            colour: GREEN,
        };
        c.number(0, i64::from(w) / 2, i64::from(h) / 2 - unit * 2, g);
    }
}
