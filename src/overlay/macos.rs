//! The overlay on macOS: a borderless AppKit window covering the main screen, above other
//! apps' windows (screen-saver level) and present on every Space including full-screen ones,
//! that ignores the mouse and never becomes key, so the game underneath keeps focus and
//! input. Pixels outside the opaque rectangles are transparent, as on the other backends.
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

/// An `NSImage` of `px` (row-major `0x00RRGGBB`, `w * h` long), `size` points big. With
/// `black_is_clear`, pure black pixels are fully transparent.
pub(crate) fn ns_image(
    px: &[u32],
    w: usize,
    h: usize,
    size: NSSize,
    black_is_clear: bool,
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
        let a = if black_is_clear && p == draw::BLACK {
            0
        } else {
            0xff
        };
        o.copy_from_slice(&[r, g, b, a]);
    }
    let ns = NSImage::initWithSize(NSImage::alloc(), size);
    ns.addRepresentation(&rep);
    Some(ns)
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

/// Open the overlay window. Must be called on the main thread.
pub fn open(_title: &str) -> Result<Box<dyn Backend>> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the macOS overlay must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon and no menu bar of our own, so the game stays the active app.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let screen = NSScreen::mainScreen(mtm).ok_or_else(|| anyhow!("no screen"))?;
    let frame = screen.frame();
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
    /// The screen in points; the scene is drawn at that size and AppKit scales it to the
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
        // With opaque rectangles the scene is the border alone, drawn on black; clearing black
        // leaves exactly the border. Without them the whole window is opaque.
        let img = ns_image(px, w as usize, h as usize, self.size, opaque.is_some())
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
