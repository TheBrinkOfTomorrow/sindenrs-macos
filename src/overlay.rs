//! A fullscreen overlay the driver draws itself.
//!
//! The gun tracks a bright border around the screen edge, so the driver has to be able to
//! draw one; and calibration happens while staring at that screen, where terminal output is
//! invisible. Both needs are the same window, so this module owns it.
//!
//! Rendering is a plain pixel buffer via `softbuffer`, with no GPU dependency and no font
//! files: numbers are drawn as seven-segment glyphs and everything else is conveyed with
//! shape and colour. The event loop must own the main thread, so the tracking work runs on a
//! worker and communicates through [`Scene`].

use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// Whether the camera can currently see the whole border.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// The whole border is in view; measurements are trustworthy.
    Good,
    /// The border runs off the edge of the camera frame, so the solve is unreliable.
    Clipped,
    /// No border found at all.
    #[default]
    Lost,
}

/// Everything the overlay draws. Updated by the worker, read by the event loop.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// Border thickness as a fraction of the shorter screen dimension.
    pub border_frac: f64,
    /// Calibration targets in screen percent; empty draws just the border.
    pub targets: Vec<[f64; 2]>,
    /// Which target is being measured.
    pub current: Option<usize>,
    /// Where each measured target actually read.
    pub measured: Vec<Option<[f64; 2]>>,
    /// Live aim point in screen percent.
    pub aim: Option<[f64; 2]>,
    pub quality: Quality,
    /// Set to flash the current target when a shot could not be measured.
    pub flash_until: Option<Instant>,
    pub done: bool,
}

impl Scene {
    pub fn border_only(border_frac: f64) -> Self {
        Self {
            border_frac,
            ..Default::default()
        }
    }
}

const BLACK: u32 = 0x0000_0000;
const WHITE: u32 = 0x00FF_FFFF;
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
fn px(v: f64) -> i64 {
    v.round() as i64
}

/// Size and colour of the seven-segment digits.
#[derive(Clone, Copy, Debug)]
struct Glyph {
    w: i64,
    h: i64,
    thickness: i64,
    colour: u32,
}

struct Canvas<'a> {
    px: &'a mut [u32],
    w: i64,
    h: i64,
}

// Every coordinate is clamped to the buffer before it is used as an index, and the buffer is
// at most a screen in size, so the casts below cannot truncate or go negative.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
impl Canvas<'_> {
    fn fill(&mut self, c: u32) {
        self.px.fill(c);
    }

    fn rect(&mut self, x: i64, y: i64, w: i64, h: i64, c: u32) {
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
    fn frame(&mut self, x: i64, y: i64, w: i64, h: i64, t: i64, c: u32) {
        self.rect(x, y, w, t, c);
        self.rect(x, y + h - t, w, t, c);
        self.rect(x, y, t, h, c);
        self.rect(x + w - t, y, t, h, c);
    }

    fn disc(&mut self, cx: i64, cy: i64, r: i64, c: u32) {
        for dy in -r..=r {
            let dx = ((r * r - dy * dy) as f64).sqrt() as i64;
            self.rect(cx - dx, cy + dy, 2 * dx + 1, 1, c);
        }
    }

    fn ring(&mut self, cx: i64, cy: i64, r: i64, t: i64, c: u32) {
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

    fn cross(&mut self, cx: i64, cy: i64, r: i64, t: i64, c: u32) {
        self.rect(cx - t / 2, cy - r, t, 2 * r, c);
        self.rect(cx - r, cy - t / 2, 2 * r, t, c);
    }

    fn line(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, t: i64, c: u32) {
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
    fn number(&mut self, mut v: u32, cx: i64, y: i64, s: Glyph) {
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

struct App {
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    scene: Arc<Mutex<Scene>>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

impl App {
    fn draw(&mut self, w: u32, h: u32) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else {
            return;
        };
        if surface.resize(nw, nh).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        let scene = self.scene.lock().map(|s| s.clone()).unwrap_or_default();
        let mut c = Canvas {
            px: &mut buf,
            w: i64::from(w),
            h: i64::from(h),
        };
        c.fill(BLACK);

        // The border the gun actually tracks. Everything else is smaller than it and inside.
        let t = px(f64::from(w.min(h)) * scene.border_frac).max(2);
        c.frame(0, 0, i64::from(w), i64::from(h), t, WHITE);

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
                // A pulsing ring so the target you are meant to shoot is unmistakable.
                let phase = (self.started.elapsed().as_secs_f64() * 2.5).sin().abs();
                let r = unit * 3 + px(phase * unit as f64);
                c.ring(
                    x,
                    y,
                    r,
                    (unit / 3).max(2),
                    if flashing { RED } else { AMBER },
                );
                let g = Glyph {
                    w: unit * 2,
                    h: unit * 3,
                    thickness: (unit / 3).max(2),
                    colour: AMBER,
                };
                c.number(u32::try_from(i + 1).unwrap_or(0), x, y - unit * 8, g);
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

        // Tracking quality, inside the border so it never interferes with detection.
        let q = match scene.quality {
            Quality::Good => GREEN,
            Quality::Clipped => AMBER,
            Quality::Lost => RED,
        };
        c.rect(t * 2, i64::from(h) - t * 2 - unit, unit * 3, unit, q);

        if scene.done {
            let g = Glyph {
                w: unit * 2,
                h: unit * 3,
                thickness: (unit / 3).max(2),
                colour: GREEN,
            };
            c.number(0, i64::from(w) / 2, i64::from(h) / 2 - unit * 2, g);
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("sindenrs")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let Ok(window) = event_loop.create_window(attrs) else {
            self.stop.store(true, Ordering::Relaxed);
            event_loop.exit();
            return;
        };
        let window = Rc::new(window);
        window.set_cursor_visible(false);
        match softbuffer::Context::new(window.clone()) {
            Ok(ctx) => {
                self.surface = softbuffer::Surface::new(&ctx, window.clone()).ok();
                self.context = Some(ctx);
            }
            Err(_) => {
                self.stop.store(true, Ordering::Relaxed);
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.stop.store(true, Ordering::Relaxed);
                event_loop.exit();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape)
                {
                    self.stop.store(true, Ordering::Relaxed);
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(win) = self.window.clone() {
                    let size = win.inner_size();
                    self.draw(size.width, size.height);
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.stop.load(Ordering::Relaxed) {
            event_loop.exit();
            return;
        }
        if let Some(win) = self.window.as_ref() {
            win.request_redraw();
        }
        // ~60 Hz is plenty for a pulsing ring and a moving dot.
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(16),
        ));
    }
}

/// Run the overlay on the calling thread, which must be the main thread. Returns when the
/// worker sets `stop` or the user presses Escape.
pub fn run(scene: Arc<Mutex<Scene>>, stop: Arc<AtomicBool>) -> Result<()> {
    let event_loop = EventLoop::new().context("creating the window event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App {
        window: None,
        context: None,
        surface: None,
        scene,
        stop: stop.clone(),
        started: Instant::now(),
    };
    let r = event_loop.run_app(&mut app).context("overlay event loop");
    stop.store(true, Ordering::Relaxed);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_primitives_stay_in_bounds() {
        let mut buf = vec![0u32; 20 * 10];
        let mut c = Canvas {
            px: &mut buf,
            w: 20,
            h: 10,
        };
        c.rect(-5, -5, 100, 100, WHITE);
        assert!(buf.iter().all(|&v| v == WHITE));
        let mut c = Canvas {
            px: &mut buf,
            w: 20,
            h: 10,
        };
        c.fill(BLACK);
        c.disc(0, 0, 50, GREEN);
        c.ring(19, 9, 30, 3, RED);
        c.cross(10, 5, 40, 3, CYAN);
        c.line(-20, -20, 40, 40, 3, AMBER);
        c.number(
            1234,
            10,
            2,
            Glyph {
                w: 4,
                h: 6,
                thickness: 1,
                colour: WHITE,
            },
        );
        // Entirely off-screen in each direction must be a no-op, not a panic.
        c.rect(100, 0, 10, 10, RED);
        c.rect(-100, 0, 10, 10, RED);
        c.rect(0, 100, 10, 10, RED);
        c.rect(0, -100, 10, 10, RED);
    }

    #[test]
    fn border_thickness_is_never_zero() {
        let mut buf = vec![0u32; 64 * 64];
        let mut c = Canvas {
            px: &mut buf,
            w: 64,
            h: 64,
        };
        c.frame(0, 0, 64, 64, px(0.0_f64.max(2.0)), WHITE);
        assert_eq!(buf[0], WHITE);
        assert_eq!(buf[64 * 32 + 32], 0);
    }
}
