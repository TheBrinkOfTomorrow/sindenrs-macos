//! `run`'s heads-up display on macOS: a reticle at each gun's aim and each gun's camera view
//! (the feed and what the detector made of it), switched from the menu bar or with ⌃⌥C / ⌃⌥P.
//!
//! It is its own window, transparent and click-through above the border, holding small image
//! views that a timer moves (the reticles) or refreshes (the panels, ~15 per second, only while
//! shown). Nothing full-screen is redrawn, so it costs next to nothing while a game runs: the
//! border overlay redraws its whole screen image on every change, which would be far too
//! heavy at 60 Hz. Like the preview page, everything is drawn dim so the camera ignores it.
//! Main thread only; it relies on whatever pumps AppKit events there (the overlay, or
//! `menubar::pump_until`) to run its timer.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSImage, NSImageScaling, NSImageView, NSScreen,
    NSScreenSaverWindowLevel, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSTimer};

use super::{aim_marker, Live};
use crate::overlay::macos::{ns_image, Clear};

/// Guns the display has room for: panels stack up from the bottom-left corner.
const MAX_GUNS: usize = 4;

/// One gun's views.
struct Slot {
    reticle: Retained<NSImageView>,
    camera: Retained<NSImageView>,
    processed: Retained<NSImageView>,
    /// The `LiveGun::serial` whose images are showing.
    serial: u64,
}

/// The window and its timer; closed on drop.
pub struct Hud {
    window: Retained<NSWindow>,
    timer: Retained<NSTimer>,
}

impl Drop for Hud {
    fn drop(&mut self) {
        self.timer.invalidate();
        self.window.orderOut(None);
    }
}

fn image_view(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSImageView> {
    let v = NSImageView::new(mtm);
    v.setFrame(frame);
    v.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    v.setHidden(true);
    v
}

/// Open the display over the main screen. Must be called on the main thread.
pub fn open(live: Arc<Mutex<Live>>) -> Result<Hud> {
    let mtm =
        MainThreadMarker::new().ok_or_else(|| anyhow!("the HUD must run on the main thread"))?;
    let screen = NSScreen::mainScreen(mtm).ok_or_else(|| anyhow!("no screen"))?;
    let frame = screen.frame();
    let (sw, sh) = (frame.size.width, frame.size.height);
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
    // SAFETY: `Hud` keeps the window alive and orders it out on drop.
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
    let content = NSView::new(mtm);
    content.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
    window.setContentView(Some(&content));

    // The reticle: the preview page's marker, about 7% of the screen height across.
    let marker_size = (sh * 0.07).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let marker = aim_marker((marker_size * 2.0) as usize);
    let marker: Retained<NSImage> = ns_image(
        &marker.px,
        marker.w,
        marker.h,
        NSSize::new(marker_size, marker_size),
        Clear::Black,
    )
    .ok_or_else(|| anyhow!("could not build the reticle"))?;

    // Panels: side by side per gun, stacked up from the bottom-left corner, inside the border.
    let (pw, gap) = (sw * 0.12, sw * 0.006);
    let ph = pw * 0.75;
    let (x0, y0) = (sw * 0.05, sh * 0.07);
    let slots: Vec<Slot> = (0..MAX_GUNS)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let y = y0 + (ph + gap) * i as f64;
            let reticle = image_view(
                mtm,
                NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(marker_size, marker_size),
                ),
            );
            reticle.setImage(Some(&marker));
            let camera = image_view(mtm, NSRect::new(NSPoint::new(x0, y), NSSize::new(pw, ph)));
            let processed = image_view(
                mtm,
                NSRect::new(NSPoint::new(x0 + pw + gap, y), NSSize::new(pw, ph)),
            );
            for v in [&camera, &processed, &reticle] {
                content.addSubview(v);
            }
            Slot {
                reticle,
                camera,
                processed,
                serial: u64::MAX,
            }
        })
        .collect();
    window.orderFrontRegardless();

    let slots = RefCell::new(slots);
    let panel_size = NSSize::new(pw, ph);
    let tick = RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| {
        let Ok(l) = live.lock() else { return };
        let mut slots = slots.borrow_mut();
        let mut guns = l.guns.values();
        for slot in slots.iter_mut() {
            let gun = guns.next();
            match gun.and_then(|g| g.aim).filter(|_| l.reticle) {
                Some([x, y]) => {
                    slot.reticle.setFrameOrigin(NSPoint::new(
                        x / 100.0 * sw - marker_size / 2.0,
                        (1.0 - y / 100.0) * sh - marker_size / 2.0,
                    ));
                    slot.reticle.setHidden(false);
                }
                None => slot.reticle.setHidden(true),
            }
            match gun.filter(|g| l.camera && g.serial > 0) {
                Some(g) => {
                    if g.serial != slot.serial {
                        slot.serial = g.serial;
                        let cam = ns_image(
                            &g.camera.px,
                            g.camera.w,
                            g.camera.h,
                            panel_size,
                            Clear::Nothing,
                        );
                        let pro = ns_image(
                            &g.processed.px,
                            g.processed.w,
                            g.processed.h,
                            panel_size,
                            Clear::Nothing,
                        );
                        slot.camera.setImage(cam.as_deref());
                        slot.processed.setImage(pro.as_deref());
                    }
                    slot.camera.setHidden(false);
                    slot.processed.setHidden(false);
                }
                None => {
                    slot.camera.setHidden(true);
                    slot.processed.setHidden(true);
                }
            }
        }
    });
    // SAFETY: a repeating timer on the main run loop whose block touches only main-thread
    // views it owns and the shared state behind its mutex.
    let timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &tick) };
    Ok(Hud { window, timer })
}
