//! The overlay on macOS: a borderless AppKit window holding the largest rectangle of the
//! display profile's aspect ratio that fits in the main screen's visible frame (below the
//! menu bar, beside the Dock), centred — so on an ultrawide the tracked border is the same
//! 16:9 rectangle the detector models and the game sits in, not the whole screen. It sits
//! above other apps' windows (screen-saver level) and is present on every Space including
//! full-screen ones, ignores the mouse and never becomes key, so the game underneath keeps
//! focus and input. Pixels outside the opaque rectangles are transparent, as on the other
//! backends. [`aim_map`] reports where the window sits, for mapping aim to gun positions.
//!
//! AppKit windows belong to the main thread, so this backend must be opened and pumped there;
//! [`Backend::pump`] runs the AppKit event loop by hand for up to its timeout. Callers that
//! also do other work (`run`) do it on other threads.

#![allow(unsafe_code)]

use std::ptr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageRep, NSColor,
    NSDeviceRGBColorSpace, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSImage,
    NSImageScaling, NSImageView, NSScreen, NSScreenSaverWindowLevel, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};

use super::draw::{self, Rect};
use super::{Backend, Event};
use crate::runtime::AimMap;

/// Which pixels of an image are see-through.
#[derive(Clone, Copy)]
pub(crate) enum Clear<'a> {
    /// None: the whole image is opaque.
    Nothing,
    /// Pure black pixels.
    Black,
    /// Everything outside these rectangles, like the shaped windows of the other backends.
    Outside(&'a [Rect]),
}

/// An `NSImage` of `px` (row-major `0x00RRGGBB`, `w * h` long), `size` points big, with the
/// pixels `clear` names fully transparent.
pub(crate) fn ns_image(
    px: &[u32],
    w: usize,
    h: usize,
    size: NSSize,
    clear: Clear<'_>,
) -> Option<Retained<NSImage>> {
    // AppKit rejects a zero-sized bitmap (a preview before its first frame).
    if w == 0 || h == 0 || px.len() < w * h {
        return None;
    }
    let (wi, hi) = (isize::try_from(w).ok()?, isize::try_from(h).ok()?);
    // SAFETY: with null planes the rep allocates its own `w * h * 4` byte buffer, which is
    // filled below through `bitmapData` before anything reads it.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            wi,
            hi,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            wi * 4,
            32,
        )
    }?;
    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    // SAFETY: the rep owns exactly `w * h * 4` bytes at `data`.
    let out = unsafe { std::slice::from_raw_parts_mut(data, w * h * 4) };
    for (o, &p) in out.chunks_exact_mut(4).zip(px) {
        let [b, g, r, _] = p.to_le_bytes();
        let a = match clear {
            Clear::Black if p == draw::BLACK => 0,
            _ => 0xff,
        };
        o.copy_from_slice(&[r, g, b, a]);
    }
    if let Clear::Outside(rects) = clear {
        for (o, k) in out.chunks_exact_mut(4).zip(inside(w, h, rects)) {
            if !k {
                o.copy_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    let ns = NSImage::initWithSize(NSImage::alloc(), size);
    ns.addRepresentation(&rep);
    Some(ns)
}

/// For each pixel of a `w`x`h` image, whether it lies inside one of `rects`.
fn inside(w: usize, h: usize, rects: &[Rect]) -> Vec<bool> {
    let mut keep = vec![false; w * h];
    for r in rects {
        let x0 = usize::try_from(r.x.max(0)).unwrap_or(0).min(w);
        let y0 = usize::try_from(r.y.max(0)).unwrap_or(0).min(h);
        let x1 = (x0 + r.w as usize).min(w);
        let y1 = (y0 + r.h as usize).min(h);
        if w == 0 || x0 >= x1 {
            continue;
        }
        for row in keep.chunks_exact_mut(w).take(y1).skip(y0) {
            row[x0..x1].fill(true);
        }
    }
    keep
}

/// Stop `-[NSApplication run]` from inside a callback: `stop:` takes effect after the next
/// event, so post one.
pub(crate) fn stop_app(app: &NSApplication) {
    app.stop(None);
    if let Some(ev) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        0.0,
        0,
        None,
        0,
        0,
        0,
    ) {
        app.postEvent_atStart(&ev, true);
    }
}

struct MacOverlay {
    app: Retained<NSApplication>,
    window: Retained<NSWindow>,
    view: Retained<NSImageView>,
    size: NSSize,
}

/// The largest `aspect` (width over height) rectangle that fits in `within`, centred, on
/// whole points. A degenerate aspect returns `within` itself.
fn fitted(within: NSRect, aspect: f64) -> NSRect {
    let (w, h) = (within.size.width, within.size.height);
    if !(aspect.is_finite() && aspect > 0.0 && w > 0.0 && h > 0.0) {
        return within;
    }
    let (fw, fh) = if w / h > aspect {
        (h * aspect, h)
    } else {
        (w, w / aspect)
    };
    NSRect::new(
        NSPoint::new(
            (within.origin.x + (w - fw) / 2.0).round(),
            (within.origin.y + (h - fh) / 2.0).round(),
        ),
        NSSize::new(fw.round(), fh.round()),
    )
}

/// The map from percent of `rect` to percent of `frame`, both in top-left-origin
/// coordinates (AppKit rects count from the bottom left; the gun's pointer, like the
/// cursor, counts from the top).
fn map_between(frame: NSRect, rect: NSRect) -> AimMap {
    AimMap {
        scale: [
            rect.size.width / frame.size.width,
            rect.size.height / frame.size.height,
        ],
        offset: [
            (rect.origin.x - frame.origin.x) / frame.size.width * 100.0,
            ((frame.origin.y + frame.size.height) - (rect.origin.y + rect.size.height))
                / frame.size.height
                * 100.0,
        ],
    }
}

/// Where the overlay window for `aspect` sits on the main screen, as the map from border
/// percent to the screen percent the gun's absolute pointer spans: apply it to an aim
/// before sending the position, and the cursor lands where the gun points even though the
/// border covers only part of the screen. Must be called on the main thread.
pub fn aim_map(aspect: f64) -> Result<AimMap> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the overlay placement must be read on the main thread"))?;
    let screen = NSScreen::mainScreen(mtm).ok_or_else(|| anyhow!("no screen"))?;
    Ok(map_between(
        screen.frame(),
        fitted(screen.visibleFrame(), aspect),
    ))
}

/// Open the overlay window, letterboxed to `aspect` within the main screen's visible
/// frame. Must be called on the main thread.
pub fn open(_title: &str, aspect: f64) -> Result<Box<dyn Backend>> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the macOS overlay must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon and no menu bar of our own, so the game stays the active app.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let screen = NSScreen::mainScreen(mtm).ok_or_else(|| anyhow!("no screen"))?;
    let frame = fitted(screen.visibleFrame(), aspect);
    // SAFETY: NSWindow's designated initializer with valid arguments.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the backend keeps the window alive and orders it out on drop.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    let view = NSImageView::new(mtm);
    view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
    view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
    window.setContentView(Some(&view));
    window.orderFrontRegardless();
    Ok(Box::new(MacOverlay {
        app,
        window,
        view,
        size: frame.size,
    }))
}

impl Backend for MacOverlay {
    /// The window in points; the scene is drawn at that size and AppKit scales it to the
    /// display's backing pixels.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn size(&self) -> (u32, u32) {
        (
            self.size.width.round() as u32,
            self.size.height.round() as u32,
        )
    }

    fn present(&mut self, px: &[u32], opaque: Option<&[Rect]>) -> Result<()> {
        let (w, h) = self.size();
        // Only the opaque rectangles exist (as on the shaped X11 and Wayland windows); the rest
        // of the scene, such as the calibration status panel, is not shown. Without them the
        // whole window is opaque.
        let clear = opaque.map_or(Clear::Nothing, Clear::Outside);
        let img = ns_image(px, w as usize, h as usize, self.size, clear)
            .ok_or_else(|| anyhow!("could not build the overlay image"))?;
        self.view.setImage(Some(&img));
        Ok(())
    }

    fn pump(&mut self, timeout: Duration) -> Result<Event> {
        let until = NSDate::dateWithTimeIntervalSinceNow(timeout.as_secs_f64());
        let now = NSDate::distantPast();
        // SAFETY: a framework constant.
        let mode = unsafe { NSDefaultRunLoopMode };
        let mut first = true;
        // Wait up to `timeout` for the first event, then drain whatever else is queued.
        while let Some(ev) = self.app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(if first { &until } else { &now }),
            mode,
            true,
        ) {
            first = false;
            self.app.sendEvent(&ev);
        }
        self.app.updateWindows();
        Ok(Event::Idle)
    }
}

impl Drop for MacOverlay {
    fn drop(&mut self) {
        self.window.orderOut(None);
    }
}

#[cfg(test)]
mod tests {
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    use super::{fitted, inside, map_between};
    use crate::overlay::draw::{self, border_rects, BLACK, WHITE};
    use crate::overlay::Scene;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    /// An ultrawide visible frame is height-limited: a 16:9 window pillarboxed in its
    /// middle; a 5:4 screen is width-limited: letterboxed. A junk aspect changes nothing.
    #[test]
    fn fitted_letterboxes_to_the_aspect() {
        let f = fitted(rect(0.0, 40.0, 3440.0, 1400.0), 2.0);
        assert_eq!(
            (f.origin.x, f.origin.y, f.size.width, f.size.height),
            (320.0, 40.0, 2800.0, 1400.0)
        );
        let f = fitted(rect(0.0, 0.0, 1280.0, 1024.0), 16.0 / 9.0);
        assert_eq!(
            (f.origin.x, f.origin.y, f.size.width, f.size.height),
            (0.0, 152.0, 1280.0, 720.0)
        );
        let within = rect(3.0, 7.0, 100.0, 50.0);
        assert_eq!(fitted(within, 0.0), within);
        assert_eq!(fitted(within, f64::NAN), within);
    }

    /// The map must send the window's corners to its screen position in top-left-origin
    /// percent: AppKit's bottom-left y has to flip.
    #[test]
    fn map_between_flips_y_and_scales() {
        // A 2800x1400 window at (320, 40) on a 3440x1440 screen: 40 points of menu bar
        // left above it (bottom-left y 0..40 is below, 1440-40-1400 = 0 above). Make the
        // gap asymmetric to catch a flip: window at bottom-left y 15, so 25 points above.
        let m = map_between(
            rect(0.0, 0.0, 3440.0, 1440.0),
            rect(320.0, 15.0, 2800.0, 1400.0),
        );
        let close =
            |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
        // Top-left of the border (0%, 0%) sits 320 points in and 25 points down.
        assert!(close(
            m.apply([0.0, 0.0]),
            [320.0 / 3440.0 * 100.0, 25.0 / 1440.0 * 100.0]
        ));
        // Bottom-right (100%, 100%) sits at 3120 points across, 1425 points down.
        assert!(close(
            m.apply([100.0, 100.0]),
            [3120.0 / 3440.0 * 100.0, 1425.0 / 1440.0 * 100.0]
        ));
    }

    /// A border-only scene still paints the calibration status panel; only the border's
    /// rectangles may show, as on the shaped Linux windows.
    #[test]
    fn only_the_border_shows() {
        let (w, h) = (640u32, 360u32);
        let mut px = vec![BLACK; (w * h) as usize];
        draw::render(&mut px, w, h, &Scene::border_only(0.03));
        let keep = inside(w as usize, h as usize, &border_rects(w, h, 0.03));
        let hidden = px
            .iter()
            .zip(&keep)
            .filter(|(&p, &k)| !k && p != BLACK)
            .count();
        assert!(
            hidden > 0,
            "the scene draws a status panel besides the border"
        );
        for (&p, &k) in px.iter().zip(&keep) {
            if k {
                assert_eq!(p, WHITE, "a kept pixel that is not border");
            }
        }
    }

    #[test]
    fn rects_are_clipped_to_the_image() {
        let r = [draw::Rect {
            x: -5,
            y: 2,
            w: 20,
            h: 50,
        }];
        let keep = inside(10, 4, &r);
        assert_eq!(keep.iter().filter(|&&k| k).count(), 10 * 2);
        assert!(inside(0, 0, &r).is_empty());
    }
}
