//! The preview page on macOS: a borderless, full-screen AppKit window above the menu bar
//! showing the border, the targets, the two preview panels, a status line and a log of every
//! click and key it receives. It runs on the main thread (AppKit's rule); the tracker feeds
//! it through [`Shared`] from a worker. Esc closes it and stops the tracker.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{
    define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageRep, NSColor,
    NSDeviceRGBColorSpace, NSEvent, NSEventModifierFlags, NSEventType, NSFont, NSFontWeightRegular,
    NSImage, NSImageScaling, NSImageView, NSResponder, NSScreen, NSScreenSaverWindowLevel,
    NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use super::{page, score, Image, Shared, TARGETS};

/// An `NSImage` holding `img` (one RGBA bitmap representation), sized `size` points.
fn ns_image(img: &Image, size: NSSize) -> Option<Retained<NSImage>> {
    // Nothing to show before the first frame (AppKit rejects a zero-sized bitmap).
    if img.w == 0 || img.h == 0 || img.px.len() < img.w * img.h {
        return None;
    }
    let (w, h) = (isize::try_from(img.w).ok()?, isize::try_from(img.h).ok()?);
    // SAFETY: with null planes the rep allocates its own `w * h * 4` byte buffer, which is
    // filled below through `bitmapData` before anything reads it.
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            w,
            h,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            w * 4,
            32,
        )
    }?;
    let data = rep.bitmapData();
    if data.is_null() {
        return None;
    }
    // SAFETY: the rep owns exactly `w * h * 4` bytes at `data`.
    let out = unsafe { std::slice::from_raw_parts_mut(data, img.w * img.h * 4) };
    for (o, &p) in out.chunks_exact_mut(4).zip(&img.px) {
        let [b, g, r, _] = p.to_le_bytes();
        o.copy_from_slice(&[r, g, b, 0xff]);
    }
    let ns = NSImage::initWithSize(NSImage::alloc(), size);
    ns.addRepresentation(&rep);
    Some(ns)
}

struct ViewIvars {
    page: Retained<NSImage>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

define_class!(
    /// The page: draws the border and targets, and turns clicks into scored shots.
    #[unsafe(super(NSView, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SindenrsPreviewPage"]
    #[ivars = ViewIvars]
    struct PageView;

    unsafe impl NSObjectProtocol for PageView {}

    impl PageView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.ivars().page.drawInRect(self.bounds());
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            self.shot(e, "left");
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            self.shot(e, "right");
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            let name = format!("button {}", e.buttonNumber());
            self.shot(e, &name);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            if e.keyCode() == 53 {
                self.ivars().stop.store(true, Ordering::Relaxed);
                return;
            }
            if e.isARepeat() {
                return;
            }
            let name = match e.keyCode() {
                123 => "left".to_owned(),
                124 => "right".to_owned(),
                125 => "down".to_owned(),
                126 => "up".to_owned(),
                _ => e
                    .charactersIgnoringModifiers()
                    .map_or_else(|| "?".to_owned(), |s| s.to_string()),
            };
            self.log(format!("key {name}"));
        }
    }
);

impl PageView {
    fn new(mtm: MainThreadMarker, frame: NSRect, ivars: ViewIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        // SAFETY: NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn log(&self, line: String) {
        if let Ok(mut s) = self.ivars().shared.lock() {
            s.push_log(line);
        }
    }

    /// Score a click (from the gun or the mouse) against the nearest target.
    fn shot(&self, e: &NSEvent, button: &str) {
        let b = self.bounds();
        let p = e.locationInWindow();
        let at = [
            p.x / b.size.width * 100.0,
            (1.0 - p.y / b.size.height) * 100.0,
        ];
        let (i, err) = score(at);
        let aim = self.ivars().shared.lock().ok().and_then(|s| s.aim);
        let mut line = format!(
            "{button:<6} at ({:5.1}%, {:5.1}%)  target {} ({:.0},{:.0})  off ({:+5.1}, {:+5.1})",
            at[0],
            at[1],
            i + 1,
            TARGETS[i][0],
            TARGETS[i][1],
            err[0],
            err[1]
        );
        if let Some(a) = aim {
            let _ = write!(line, "  tracker ({:5.1}%, {:5.1}%)", a[0], a[1]);
        }
        self.log(line);
    }
}

define_class!(
    /// A borderless window that can still take key events.
    #[unsafe(super(NSWindow, NSResponder, objc2_foundation::NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SindenrsPreviewWindow"]
    struct KeyWindow;

    impl KeyWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }
    }
);

fn label(mtm: MainThreadMarker, frame: NSRect, size: f64, white: f64) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    // SAFETY: a framework constant.
    let weight = unsafe { NSFontWeightRegular };
    l.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
        size, weight,
    )));
    l.setTextColor(Some(&NSColor::colorWithWhite_alpha(white, 1.0)));
    l.setFrame(frame);
    l
}

/// Show the page on the main screen until the tracker is done (`shared.done`) or Esc is
/// pressed (which sets `stop`). Must be called on the main thread.
pub fn run(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, border_frac: f64) -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow!("the preview must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let screen = NSScreen::mainScreen(mtm).ok_or_else(|| anyhow!("no screen"))?;
    let frame = screen.frame();
    let (sw, sh) = (frame.size.width, frame.size.height);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let page_img = page(sw.round() as u32, sh.round() as u32, border_frac);
    let page_ns =
        ns_image(&page_img, frame.size).ok_or_else(|| anyhow!("could not build the page image"))?;

    let window = {
        let w = mtm.alloc::<KeyWindow>().set_ivars(());
        // SAFETY: NSWindow's designated initializer with valid arguments.
        let w: Retained<KeyWindow> = unsafe {
            msg_send![super(w), initWithContentRect: frame, styleMask: NSWindowStyleMask::Borderless, backing: NSBackingStoreType::Buffered, defer: false]
        };
        w
    };
    // SAFETY: we keep the window alive ourselves.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setLevel(NSScreenSaverWindowLevel);
    window.setBackgroundColor(Some(&NSColor::blackColor()));

    let view = PageView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), frame.size),
        ViewIvars {
            page: page_ns,
            shared: shared.clone(),
            stop: stop.clone(),
        },
    );
    window.setContentView(Some(&view));

    // Panels and text between the lower targets (which sit at 20% and 80% across, 80% down).
    let (pw, ph) = (sw * 0.12, sw * 0.09);
    let gap = sw * 0.01;
    let panel_y = sh * 0.10;
    let left = sw * 0.5 - pw - gap / 2.0;
    let camera_view = NSImageView::new(mtm);
    camera_view.setFrame(NSRect::new(
        NSPoint::new(left, panel_y),
        NSSize::new(pw, ph),
    ));
    camera_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    let processed_view = NSImageView::new(mtm);
    processed_view.setFrame(NSRect::new(
        NSPoint::new(left + pw + gap, panel_y),
        NSSize::new(pw, ph),
    ));
    processed_view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    view.addSubview(&camera_view);
    view.addSubview(&processed_view);
    let text_w = pw * 2.0 + gap;
    let status = label(
        mtm,
        NSRect::new(
            NSPoint::new(left, panel_y + ph + gap),
            NSSize::new(text_w, sh * 0.03),
        ),
        sh * 0.011,
        0.35,
    );
    let log = label(
        mtm,
        NSRect::new(
            NSPoint::new(sw * 0.30, sh * 0.30),
            NSSize::new(sw * 0.40, sh * 0.16),
        ),
        sh * 0.010,
        0.30,
    );
    view.addSubview(&status);
    view.addSubview(&log);

    window.makeKeyAndOrderFront(None);
    window.makeFirstResponder(Some(&view));
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let seen = Cell::new(u64::MAX);
    let panel_size = NSSize::new(pw, ph);
    let app_t = app.clone();
    let tick = RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| {
        let Ok(s) = shared.lock() else { return };
        if s.serial != seen.get() {
            seen.set(s.serial);
            if let Some(i) = ns_image(&s.camera, panel_size) {
                camera_view.setImage(Some(&i));
            }
            if let Some(i) = ns_image(&s.processed, panel_size) {
                processed_view.setImage(Some(&i));
            }
        }
        status.setStringValue(&NSString::from_str(&s.status));
        let text: Vec<&str> = s.log.iter().map(String::as_str).collect();
        log.setStringValue(&NSString::from_str(&text.join("\n")));
        if s.done || stop.load(Ordering::Relaxed) {
            stop.store(true, Ordering::Relaxed);
            app_t.stop(None);
            // `stop` takes effect after the next event; post one so it happens now.
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
                app_t.postEvent_atStart(&ev, true);
            }
        }
    });
    // SAFETY: a repeating timer on the main run loop with a block that only touches
    // main-thread objects it owns.
    let timer =
        unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 30.0, true, &tick) };
    app.run();
    timer.invalidate();
    window.orderOut(None);
    Ok(())
}
