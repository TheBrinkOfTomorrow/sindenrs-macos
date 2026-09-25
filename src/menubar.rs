//! macOS menu bar control for `run`: a status item (crosshair icon) whose menu shows each
//! gun's status and offers Show Border, Show Reticle, Show Camera View and Quit, and global
//! shortcuts (⌃⌥C reticle, ⌃⌥P camera view, besides the two below) that work from anywhere,
//! games included: ⌃⌥B shows or hides the border (close to Alt-B in the vendor's Windows software; macOS
//! ignores Option-only hot keys)
//! and ⌃⌥⌘Q quits. Quitting sets the same stop flag as Ctrl-C, so `run` shuts down cleanly.
//!
//! The hotkey uses Carbon's `RegisterEventHotKey`, which needs no Accessibility or Input
//! Monitoring permission (unlike an `NSEvent` global monitor). Both it and the menu are
//! served by whatever pumps AppKit events on the main thread: the overlay, or [`pump_until`].

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn,
    NSEventMask, NSEventModifierFlags, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSObject, NSObjectProtocol, NSString};

use crate::overlay::Scene;
use crate::preview::Live;

/// Carbon's hot key API, which has no Rust bindings. Still supported, and the only way to get
/// a system-wide shortcut without asking for Accessibility access.
mod carbon {
    use std::ffi::c_void;

    #[repr(C)]
    pub struct EventHotKeyID {
        pub signature: u32,
        pub id: u32,
    }

    #[repr(C)]
    pub struct EventTypeSpec {
        pub event_class: u32,
        pub event_kind: u32,
    }

    pub type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

    /// `kEventClassKeyboard` ('keyb') and `kEventHotKeyPressed`.
    pub const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
    pub const HOT_KEY_PRESSED: u32 = 5;
    /// `kEventParamDirectObject` ('----') and `typeEventHotKeyID` ('hkid'): which hot key fired.
    pub const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    pub const HOT_KEY_ID_TYPE: u32 = u32::from_be_bytes(*b"hkid");
    /// Modifier bits (`cmdKey`, `optionKey`, `controlKey`) and `kVK_ANSI_Q`, `kVK_ANSI_B`.
    pub const CMD: u32 = 1 << 8;
    pub const OPTION: u32 = 1 << 11;
    pub const CONTROL: u32 = 1 << 12;
    pub const KEY_Q: u32 = 0x0c;
    pub const KEY_B: u32 = 0x0b;
    /// `kVK_ANSI_C`, `kVK_ANSI_P`.
    pub const KEY_C: u32 = 0x08;
    pub const KEY_P: u32 = 0x23;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        pub fn GetApplicationEventTarget() -> *mut c_void;
        pub fn InstallEventHandler(
            target: *mut c_void,
            handler: Handler,
            count: usize,
            types: *const EventTypeSpec,
            user_data: *mut c_void,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn RemoveEventHandler(handler: *mut c_void) -> i32;
        pub fn RegisterEventHotKey(
            key_code: u32,
            modifiers: u32,
            id: EventHotKeyID,
            target: *mut c_void,
            options: u32,
            out: *mut *mut c_void,
        ) -> i32;
        pub fn UnregisterEventHotKey(hot_key: *mut c_void) -> i32;
        pub fn GetEventParameter(
            event: *mut c_void,
            name: u32,
            desired_type: u32,
            actual_type: *mut u32,
            buffer_size: usize,
            actual_size: *mut usize,
            data: *mut c_void,
        ) -> i32;
    }
}

/// Hot key ids, as registered below.
const QUIT: u32 = 1;
const TOGGLE_BORDER: u32 = 2;
const TOGGLE_RETICLE: u32 = 3;
const TOGGLE_CAMERA: u32 = 4;

/// What the hot keys act on; boxed so the handler can hold a stable pointer to it.
struct HotKeys {
    stop: Arc<AtomicBool>,
    scene: Arc<Mutex<Scene>>,
    live: Arc<Mutex<Live>>,
}

/// Flip the heads-up display's reticle or camera view, and log it.
fn toggle_live(live: &Mutex<Live>, reticle: bool) {
    if let Ok(mut l) = live.lock() {
        let (on, what, key) = if reticle {
            l.reticle = !l.reticle;
            (l.reticle, "reticle", "⌃⌥C")
        } else {
            l.camera = !l.camera;
            (l.camera, "camera view", "⌃⌥P")
        };
        tracing::info!("{key}: {what} {}", if on { "shown" } else { "hidden" });
    }
}

/// A hot key fired: find out which, and act on the `HotKeys` that `user` points at.
extern "C" fn on_hot_key(_call: *mut c_void, event: *mut c_void, user: *mut c_void) -> i32 {
    let mut id = carbon::EventHotKeyID {
        signature: 0,
        id: 0,
    };
    // SAFETY: `event` is the hot key event Carbon passes us, and `id` is an `EventHotKeyID`
    // sized buffer; `user` is the `HotKeys` that `MenuBar` keeps boxed for as long as the
    // handler is installed.
    unsafe {
        let st = carbon::GetEventParameter(
            event,
            carbon::DIRECT_OBJECT,
            carbon::HOT_KEY_ID_TYPE,
            ptr::null_mut(),
            std::mem::size_of::<carbon::EventHotKeyID>(),
            ptr::null_mut(),
            ptr::addr_of_mut!(id).cast(),
        );
        if st != 0 {
            tracing::warn!("hot key pressed, but its id could not be read: {st}");
            return st;
        }
        let keys = &*user.cast::<HotKeys>();
        match id.id {
            QUIT => {
                tracing::info!("⌃⌥⌘Q: quitting");
                keys.stop.store(true, Ordering::Relaxed);
            }
            TOGGLE_BORDER => {
                if let Ok(mut s) = keys.scene.lock() {
                    s.hidden = !s.hidden;
                    tracing::info!("⌃⌥B: border {}", if s.hidden { "hidden" } else { "shown" });
                }
            }
            TOGGLE_RETICLE => toggle_live(&keys.live, true),
            TOGGLE_CAMERA => toggle_live(&keys.live, false),
            other => tracing::warn!("unknown hot key id {other}"),
        }
    }
    0
}

/// Register one global hot key, logging (not failing) if another app already owns it.
fn register(key: u32, modifiers: u32, id: u32, target: *mut c_void, name: &str) -> *mut c_void {
    let mut out = ptr::null_mut();
    // SAFETY: plain values and an out pointer for the ref.
    let st = unsafe {
        carbon::RegisterEventHotKey(
            key,
            modifiers,
            carbon::EventHotKeyID {
                signature: u32::from_be_bytes(*b"Sndn"),
                id,
            },
            target,
            0,
            &mut out,
        )
    };
    if st == 0 {
        tracing::info!("{name} registered");
    } else {
        tracing::warn!(
            "{name} unavailable (taken by another app?): RegisterEventHotKey returned {st}"
        );
    }
    out
}

struct TargetIvars {
    stop: Arc<AtomicBool>,
    scene: Arc<Mutex<Scene>>,
    status: Arc<Mutex<Vec<String>>>,
    live: Arc<Mutex<Live>>,
    /// Whether this `run` draws a border at all (no Show Border item otherwise).
    overlay: bool,
}

define_class!(
    /// Target of the menu's actions, and its delegate, so the status lines are fresh each
    /// time the menu opens.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SindenrsMenuTarget"]
    #[ivars = TargetIvars]
    struct Target;

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSMenuDelegate for Target {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            self.fill(menu);
        }
    }

    impl Target {
        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            self.ivars().stop.store(true, Ordering::Relaxed);
        }

        #[unsafe(method(toggleBorder:))]
        fn toggle_border(&self, _sender: Option<&AnyObject>) {
            if let Ok(mut s) = self.ivars().scene.lock() {
                s.hidden = !s.hidden;
            }
        }

        #[unsafe(method(toggleReticle:))]
        fn toggle_reticle(&self, _sender: Option<&AnyObject>) {
            toggle_live(&self.ivars().live, true);
        }

        #[unsafe(method(toggleCamera:))]
        fn toggle_camera(&self, _sender: Option<&AnyObject>) {
            toggle_live(&self.ivars().live, false);
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker, ivars: TargetIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn item(
        &self,
        title: &str,
        action: Option<objc2::runtime::Sel>,
        key: &str,
    ) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        // SAFETY: a plain menu item; its target (self) outlives the menu, which `MenuBar`
        // owns alongside it.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                action,
                &NSString::from_str(key),
            )
        };
        if action.is_some() {
            // SAFETY: see above.
            unsafe { item.setTarget(Some(self)) };
        } else {
            item.setEnabled(false);
        }
        item
    }

    /// Rebuild the menu: status lines, Show Border / Reticle / Camera View, Quit.
    fn fill(&self, menu: &NSMenu) {
        let mtm = self.mtm();
        let iv = self.ivars();
        menu.removeAllItems();
        menu.addItem(&self.item("Sindenrs", None, ""));
        let lines = iv.status.lock().map(|s| s.clone()).unwrap_or_default();
        if lines.is_empty() {
            menu.addItem(&self.item("Starting…", None, ""));
        }
        for line in &lines {
            menu.addItem(&self.item(line, None, ""));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        if iv.overlay {
            let border = self.item("Show Border", Some(sel!(toggleBorder:)), "b");
            border.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Control | NSEventModifierFlags::Option,
            );
            let hidden = iv.scene.lock().map(|s| s.hidden).unwrap_or(false);
            border.setState(if hidden {
                NSControlStateValueOff
            } else {
                NSControlStateValueOn
            });
            menu.addItem(&border);
        }
        let (reticle_on, camera_on) = iv
            .live
            .lock()
            .map(|l| (l.reticle, l.camera))
            .unwrap_or_default();
        for (title, action, key, on) in [
            ("Show Reticle", sel!(toggleReticle:), "c", reticle_on),
            ("Show Camera View", sel!(toggleCamera:), "p", camera_on),
        ] {
            let item = self.item(title, Some(action), key);
            item.setKeyEquivalentModifierMask(
                NSEventModifierFlags::Control | NSEventModifierFlags::Option,
            );
            item.setState(if on {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            menu.addItem(&item);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let quit = self.item("Quit Sindenrs", Some(sel!(quit:)), "q");
        quit.setKeyEquivalentModifierMask(
            NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Command,
        );
        menu.addItem(&quit);
    }
}

/// The status item, its menu and the global shortcuts; removed on drop.
pub struct MenuBar {
    item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<Target>,
    hot_keys: Vec<*mut c_void>,
    handler: *mut c_void,
    /// Kept alive (and in place) for the handler until it is removed.
    keys: Box<HotKeys>,
}

impl MenuBar {
    /// Install the menu bar item and the shortcuts: ⌃⌥B (border on/off, with an overlay),
    /// ⌃⌥C (reticle), ⌃⌥P (camera view) and ⌃⌥⌘Q (quit). `status` holds one line per gun,
    /// written by the gun supervisor; `live` is the heads-up display's state; `overlay` says
    /// whether there is a border to show or hide. Must be called on the main thread.
    pub fn install(
        stop: Arc<AtomicBool>,
        scene: Arc<Mutex<Scene>>,
        status: Arc<Mutex<Vec<String>>>,
        live: Arc<Mutex<Live>>,
        overlay: bool,
    ) -> Result<Self> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| anyhow!("the menu bar item must be created on the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let keys = Box::new(HotKeys {
            stop: stop.clone(),
            scene: scene.clone(),
            live: live.clone(),
        });
        let target = Target::new(
            mtm,
            TargetIvars {
                stop,
                scene,
                status,
                live,
                overlay,
            },
        );
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
        target.fill(&menu);

        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("scope"),
                Some(&NSString::from_str("Sindenrs")),
            );
            if let Some(icon) = icon {
                icon.setTemplate(true);
                button.setImage(Some(&icon));
            } else {
                button.setTitle(&NSString::from_str("Sinden"));
            }
        }
        item.setMenu(Some(&menu));

        let mut handler = ptr::null_mut();
        let mut hot_keys = Vec::new();
        let types = [carbon::EventTypeSpec {
            event_class: carbon::KEYBOARD,
            event_kind: carbon::HOT_KEY_PRESSED,
        }];
        // SAFETY: the application event target, a handler with the C signature Carbon
        // expects, and user data (the boxed `HotKeys`) that `MenuBar` keeps alive, unmoved,
        // until it removes the handler in `drop`.
        unsafe {
            let target = carbon::GetApplicationEventTarget();
            let st = carbon::InstallEventHandler(
                target,
                on_hot_key,
                types.len(),
                types.as_ptr(),
                ptr::from_ref::<HotKeys>(&keys).cast_mut().cast(),
                &mut handler,
            );
            if st == 0 {
                hot_keys.push(register(
                    carbon::KEY_Q,
                    carbon::CONTROL | carbon::OPTION | carbon::CMD,
                    QUIT,
                    target,
                    "⌃⌥⌘Q",
                ));
                if overlay {
                    // Control as well as Option: since macOS 15, hot keys whose only
                    // modifiers are Option (or Option-Shift) register fine but never fire.
                    hot_keys.push(register(
                        carbon::KEY_B,
                        carbon::CONTROL | carbon::OPTION,
                        TOGGLE_BORDER,
                        target,
                        "⌃⌥B",
                    ));
                }
                for (key, id, name) in [
                    (carbon::KEY_C, TOGGLE_RETICLE, "⌃⌥C"),
                    (carbon::KEY_P, TOGGLE_CAMERA, "⌃⌥P"),
                ] {
                    hot_keys.push(register(
                        key,
                        carbon::CONTROL | carbon::OPTION,
                        id,
                        target,
                        name,
                    ));
                }
            } else {
                tracing::warn!("shortcuts unavailable: InstallEventHandler returned {st}");
            }
        }
        Ok(Self {
            item,
            _menu: menu,
            _target: target,
            hot_keys,
            handler,
            keys,
        })
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        // SAFETY: refs we registered, released once; `keys` stays alive until after.
        unsafe {
            for &k in &self.hot_keys {
                if !k.is_null() {
                    carbon::UnregisterEventHotKey(k);
                }
            }
            if !self.handler.is_null() {
                carbon::RemoveEventHandler(self.handler);
            }
        }
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        let _ = &self.keys;
    }
}

/// Pump AppKit events on the main thread until `stop` is set, for `run` without an overlay:
/// the menu bar item and the shortcut still need an event loop.
pub fn pump_until(stop: &AtomicBool) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("must run on the main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();
    // SAFETY: a framework constant.
    let mode = unsafe { NSDefaultRunLoopMode };
    while !stop.load(Ordering::Relaxed) {
        let until = NSDate::dateWithTimeIntervalSinceNow(Duration::from_millis(100).as_secs_f64());
        while let Some(ev) = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            mode,
            true,
        ) {
            app.sendEvent(&ev);
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
    }
    Ok(())
}
