//! The on-screen overlay: the border the gun tracks, and the calibration UI.
//!
//! The gun tracks a bright border around the screen edge, so the driver draws one itself,
//! and it has to stay on screen while a game runs. That means a window that is always on
//! top, shaped so that only the border (and, while calibrating, the UI) exists at all, and
//! that never takes keyboard or mouse input, so everything reaches the game underneath.
//! Each window system does that its own way; the backends live in `x11` and `wayland`
//! behind [`Backend`], and the scene is rendered on the CPU by [`draw`]. Nothing here
//! needs the main thread.

pub mod artwork;
pub mod draw;
pub mod scene;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

pub use draw::Rect;
pub use scene::{Quality, Scene, SolveInfo};

/// What a backend's event pump reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Nothing happened within the timeout.
    Idle,
    /// The window was exposed or resized; draw again.
    Redraw,
    /// The window is gone (compositor closed it, connection lost).
    Closed,
}

/// One window on one window system.
pub trait Backend {
    /// Current window size in pixels.
    fn size(&self) -> (u32, u32);
    /// Show `px` (row-major `0x00RRGGBB`, `w * h` long, matching [`Backend::size`]).
    /// `opaque` lists the rectangles that should exist on screen; everything outside them
    /// is transparent and passes input through. `None` means the whole window is opaque.
    fn present(&mut self, px: &[u32], opaque: Option<&[Rect]>) -> Result<()>;
    /// Handle window-system events for up to `timeout`.
    fn pump(&mut self, timeout: Duration) -> Result<Event>;
}

/// Open a window on whatever window system the environment names: Wayland first (a
/// Wayland session usually offers X11 through XWayland too, but the layer shell is the
/// only way to stay above a fullscreen game there), then X11.
#[cfg(target_os = "linux")]
pub fn open(title: &str) -> Result<Box<dyn Backend>> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match wayland::open(title) {
            Ok(b) => return Ok(b),
            Err(e) if std::env::var_os("DISPLAY").is_some() => {
                tracing::warn!("wayland overlay unavailable ({e:#}); trying X11");
            }
            Err(e) => return Err(e),
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        return x11::open(title);
    }
    bail!("no display: neither WAYLAND_DISPLAY nor DISPLAY is set")
}

#[cfg(not(target_os = "linux"))]
pub fn open(_title: &str) -> Result<Box<dyn Backend>> {
    bail!("the overlay is not implemented on this platform yet")
}

/// Show `scene` until `stop` is set or the window goes away. Redraws only when the scene
/// changes (or the window asks), so a static border costs nothing while a game runs.
pub fn run(scene: Arc<Mutex<Scene>>, stop: Arc<AtomicBool>) -> Result<()> {
    let r = run_inner(&scene, &stop);
    stop.store(true, Ordering::Relaxed);
    r
}

fn run_inner(scene: &Mutex<Scene>, stop: &AtomicBool) -> Result<()> {
    let mut win = open("sindenrs")?;
    let mut px: Vec<u32> = Vec::new();
    let mut last: Option<(Scene, (u32, u32))> = None;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let mut cur = scene.lock().map(|s| s.clone()).unwrap_or_default();
        // A flash is a timed state; once it is over the scene is different again.
        if cur.flash_until.is_some_and(|u| u <= now) {
            cur.flash_until = None;
        }
        let size = win.size();
        let ev = win.pump(Duration::from_millis(16))?;
        if ev == Event::Closed {
            break;
        }
        let changed = last.as_ref().is_none_or(|(s, sz)| *s != cur || *sz != size);
        if !(changed || ev == Event::Redraw) {
            continue;
        }
        let (w, h) = size;
        if w == 0 || h == 0 {
            continue;
        }
        px.clear();
        px.resize(w as usize * h as usize, 0);
        draw::render(&mut px, w, h, &cur);
        // Only the border exists while just tracking; the calibration UI needs its black
        // backdrop, so then the whole window is opaque.
        let rects;
        let opaque = if cur.targets.is_empty() && cur.aim.is_none() {
            rects = draw::border_rects(w, h, cur.border_frac);
            Some(rects.as_slice())
        } else {
            None
        };
        win.present(&px, opaque)?;
        last = Some((cur, size));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::draw::{border_rects, draw_border, px, Canvas, Glyph, BLACK, WHITE};

    /// The border the overlay draws must be one the detector decodes: render it, take it
    /// as a camera frame, and check every side and most tabs come back.
    #[test]
    fn drawn_border_decodes() {
        use crate::vision::acquire::{acquire, AcquireParams};
        let (w, h) = (1280u32, 720u32);
        let mut buf = vec![0u32; (w * h) as usize];
        let mut c = Canvas {
            px: &mut buf,
            w: i64::from(w),
            h: i64::from(h),
        };
        c.fill(BLACK);
        draw_border(&mut c, w, h, 0.03);
        // Seen from a little further back: the border inside a black margin.
        let (m, fw, fh) = (60usize, 1280 + 120, 720 + 120);
        let mut luma = vec![5u8; fw * fh];
        for y in 0..h as usize {
            for x in 0..w as usize {
                if buf[y * w as usize + x] == WHITE {
                    luma[(y + m) * fw + x + m] = 230;
                }
            }
        }
        let q = acquire(&luma, fw, fh, &AcquireParams::default()).expect("found");
        assert!(q.from_lines && q.sides == 0b1111, "{q:?}");
        assert!(q.tabs >= 24, "only {} tabs decoded", q.tabs);
        let want = [[60.0, 60.0], [1339.0, 60.0], [1339.0, 779.0], [60.0, 779.0]];
        for (got, want) in q.corners.iter().zip(want) {
            assert!(
                (got[0] - want[0]).abs() < 3.0 && (got[1] - want[1]).abs() < 3.0,
                "{got:?} vs {want:?}"
            );
        }
    }

    /// The shape rectangles must cover exactly the pixels the border paints, or the game
    /// would show through a gap in the border (or be hidden behind an invisible strip).
    #[test]
    #[allow(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    fn border_rects_match_drawn_border() {
        let (w, h) = (1280u32, 960u32);
        let mut buf = vec![0u32; (w * h) as usize];
        let mut c = Canvas {
            px: &mut buf,
            w: i64::from(w),
            h: i64::from(h),
        };
        draw_border(&mut c, w, h, 0.03);
        let mut covered = vec![false; (w * h) as usize];
        for r in border_rects(w, h, 0.03) {
            for y in r.y..r.y + r.h as i32 {
                for x in r.x..r.x + r.w as i32 {
                    covered[(y as u32 * w + x as u32) as usize] = true;
                }
            }
        }
        for (i, (&p, &cov)) in buf.iter().zip(&covered).enumerate() {
            assert_eq!(
                p == WHITE,
                cov,
                "pixel {} ({}, {})",
                i,
                i as u32 % w,
                i as u32 / w
            );
        }
    }

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
        c.disc(0, 0, 50, 0x00FF_0000);
        c.ring(19, 9, 30, 3, 0x00FF_0000);
        c.cross(10, 5, 40, 3, 0x00FF_0000);
        c.line(-20, -20, 40, 40, 3, 0x00FF_0000);
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
        c.rect(100, 0, 10, 10, WHITE);
        c.rect(-100, 0, 10, 10, WHITE);
        c.rect(0, 100, 10, 10, WHITE);
        c.rect(0, -100, 10, 10, WHITE);
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
