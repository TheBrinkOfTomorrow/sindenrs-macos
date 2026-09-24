//! macOS menu bar control for `run`: a status item (crosshair icon) whose menu shows each
//! gun's status and offers Show Border and Quit, and a global ⌃⌥⌘Q that quits from anywhere,
//! games included. Quitting sets the same stop flag as Ctrl-C, so `run` shuts down cleanly.
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
    /// Modifier bits (`cmdKey`, `optionKey`, `controlKey`) and `kVK_ANSI_Q`.
    pub const CMD: u32 = 1 << 8;
    pub const OPTION: u32 = 1 << 11;
    pub const CONTROL: u32 = 1 << 12;
    pub const KEY_Q: u32 = 0x0c;

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
    }
}

/// The hot key fired: set the stop flag `user` points at.
extern "C" fn on_hot_key(_call: *mut c_void, _event: *mut c_void, user: *mut c_void) -> i32 {
    // SAFETY: `user` is the `AtomicBool` of the `Arc` that `MenuBar` keeps alive for as long
    // as the handler is installed.
    unsafe { (*user.cast::<AtomicBool>()).store(true, Ordering::Relaxed) };
    0
}

struct TargetIvars {
    stop: Arc<AtomicBool>,
    scene: Arc<Mutex<Scene>>,
    status: Arc<Mutex<Vec<String>>>,
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

    /// Rebuild the menu: status lines, Show Border, Quit.
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
            let border = self.item("Show Border", Some(sel!(toggleBorder:)), "");
            let hidden = iv.scene.lock().map(|s| s.hidden).unwrap_or(false);
            border.setState(if hidden {
                NSControlStateValueOff
            } else {
                NSControlStateValueOn
            });
            menu.addItem(&border);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let quit = self.item("Quit Sindenrs", Some(sel!(quit:)), "q");
        quit.setKeyEquivalentModifierMask(
            NSEventModifierFlags::Control
                | NSEventModifierFlags::Option
                | NSEventModifierFlags::Command,
        );
        menu.addItem(&quit);
    }
}

/// The status item, its menu and the global shortcut; removed on drop.
pub struct MenuBar {
    item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<Target>,
    hot_key: *mut c_void,
    handler: *mut c_void,
    stop: Arc<AtomicBool>,
}

impl MenuBar {
    /// Install the menu bar item and ⌃⌥⌘Q. `status` holds one line per gun, written by the
    /// gun supervisor; `overlay` says whether there is a border to show or hide. Must be
    /// called on the main thread.
    pub fn install(
        stop: Arc<AtomicBool>,
        scene: Arc<Mutex<Scene>>,
        status: Arc<Mutex<Vec<String>>>,
        overlay: bool,
    ) -> Result<Self> {
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| anyhow!("the menu bar item must be created on the main thread"))?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

        let target = Target::new(
            mtm,
            TargetIvars {
                stop: stop.clone(),
                scene,
                status,
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

        let (mut handler, mut hot_key) = (ptr::null_mut(), ptr::null_mut());
        let types = [carbon::EventTypeSpec {
            event_class: carbon::KEYBOARD,
            event_kind: carbon::HOT_KEY_PRESSED,
        }];
        // SAFETY: the application event target, a handler with the C signature Carbon
        // expects, and user data (the stop flag) that `MenuBar` keeps alive until it removes
        // the handler in `drop`.
        unsafe {
            let target = carbon::GetApplicationEventTarget();
            let st = carbon::InstallEventHandler(
                target,
                on_hot_key,
                types.len(),
                types.as_ptr(),
                Arc::as_ptr(&stop).cast_mut().cast(),
                &mut handler,
            );
            if st != 0 {
                tracing::warn!("⌃⌥⌘Q unavailable: InstallEventHandler returned {st}");
            } else {
                let st = carbon::RegisterEventHotKey(
                    carbon::KEY_Q,
                    carbon::CONTROL | carbon::OPTION | carbon::CMD,
                    carbon::EventHotKeyID {
                        signature: u32::from_be_bytes(*b"Sndn"),
                        id: 1,
                    },
                    target,
                    0,
                    &mut hot_key,
                );
                if st != 0 {
                    tracing::warn!("⌃⌥⌘Q unavailable (taken by another app?): RegisterEventHotKey returned {st}");
                }
            }
        }
        Ok(Self {
            item,
            _menu: menu,
            _target: target,
            hot_key,
            handler,
            stop,
        })
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        // SAFETY: refs we registered, released once; the stop flag stays alive until after.
        unsafe {
            if !self.hot_key.is_null() {
                carbon::UnregisterEventHotKey(self.hot_key);
            }
            if !self.handler.is_null() {
                carbon::RemoveEventHandler(self.handler);
            }
        }
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
        let _ = &self.stop;
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
