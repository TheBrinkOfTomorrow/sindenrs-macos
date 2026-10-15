//! Moving the macOS system cursor from the gun's aim.
//!
//! On Windows and Linux the gun presents an *absolute* HID mouse: the driver streams it a
//! position (command 40) and the OS moves the system cursor to match, for free. macOS has no
//! driver that turns an absolute-coordinate HID pointer into cursor movement — it ignores the
//! coordinates (or treats them as relative deltas), and the cursor pegs in a corner. So on
//! macOS the driver must move the cursor itself, which is what this module does: it posts Core
//! Graphics mouse events at the point the tracker already computed, plus the mouse buttons the
//! gun maps by default — the trigger as a left click, front-left as a right click, front-right
//! as a middle click — each driven by its bit in the serial button stream.
//!
//! Because the gun's own HID pointer would fight this, the tracker stops streaming position
//! (command 40) to the gun on macOS, and the HID mouse buttons are masked
//! ([`crate::config::Buttons::mask_synthesised`]) so they do not fire a second, mispositioned
//! click. Everything else (recoil, keys mapped through the gun's HID keyboard) is unchanged.
//!
//! Posting events that reach another app (the browser running the game) needs the app to be
//! granted Accessibility in System Settings > Privacy & Security. Without it the cursor still
//! *moves* (the warp below does not need the grant) but clicks may be dropped, so we check once
//! and say so.

#![cfg(target_os = "macos")]
#![allow(unsafe_code)]

use anyhow::{anyhow, Result};
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use tracing::{info, warn};

// AXIsProcessTrusted lives in the ApplicationServices umbrella framework. It reports whether
// this process may post events into other apps; it returns a Core Foundation `Boolean`
// (an unsigned char), so we take it as a byte and test for non-zero.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
}

/// Moves the macOS system cursor and drives its left button, in the screen's global
/// coordinate space (points, top-left origin — the space Core Graphics mouse events use and
/// the space [`crate::runtime::AimMap`] already produces percentages of).
pub struct Cursor {
    source: CGEventSource,
    /// Main display size in points.
    width: f64,
    height: f64,
    /// The last point the cursor was moved to, so a click lands where the gun points.
    last: CGPoint,
    /// Whether each synthesised button is currently held. A held button turns moves into drags,
    /// which is what a game expects while firing; Drop releases any still down at session end.
    left_down: bool,
    right_down: bool,
    middle_down: bool,
}

/// A mouse button the macOS driver synthesises from a serial button bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SynthButton {
    Left,
    Right,
    Middle,
}

impl Cursor {
    /// Build a cursor driver bound to the main display. Logs once if the process is not
    /// trusted for Accessibility, since synthesised clicks need that grant to reach the game.
    pub fn new() -> Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow!("could not create a Core Graphics event source"))?;
        let bounds = CGDisplay::main().bounds();
        // SAFETY: a parameterless C call into a system framework.
        if unsafe { AXIsProcessTrusted() } == 0 {
            warn!(
                "this app is not trusted for Accessibility, so synthesised clicks may not reach \
                 the game — grant it under System Settings > Privacy & Security > Accessibility \
                 (the cursor itself will still move)"
            );
        } else {
            info!("Accessibility granted; driving the macOS cursor from the gun");
        }
        Ok(Self {
            source,
            width: bounds.size.width,
            height: bounds.size.height,
            last: CGPoint::new(bounds.size.width / 2.0, bounds.size.height / 2.0),
            left_down: false,
            right_down: false,
            middle_down: false,
        })
    }

    /// Place the cursor at `x`/`y` percent of the screen (0..100, the output of
    /// [`crate::runtime::AimMap::apply`]). Moves while the trigger is held are posted as drags.
    pub fn move_to(&mut self, x_percent: f64, y_percent: f64) -> Result<()> {
        let point = CGPoint::new(
            (x_percent / 100.0 * self.width).clamp(0.0, self.width - 1.0),
            (y_percent / 100.0 * self.height).clamp(0.0, self.height - 1.0),
        );
        self.last = point;
        // Warp first so the visible cursor tracks even without the Accessibility grant, then
        // post the move so a trusted app receives it as a real event.
        let _ = CGDisplay::warp_mouse_cursor_position(point);
        // A move while a button is held must be posted as that button's drag, or the game sees
        // the cursor jump without the button coming along.
        let (kind, button) = if self.left_down {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.right_down {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if self.middle_down {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post(kind, point, button)
    }

    /// Press or release one synthesised mouse button at the current point. A no-op if the button
    /// is already in that state, so repeated button reports are cheap.
    pub fn set_button(&mut self, button: SynthButton, down: bool) -> Result<()> {
        match button {
            SynthButton::Left if self.left_down == down => return Ok(()),
            SynthButton::Right if self.right_down == down => return Ok(()),
            SynthButton::Middle if self.middle_down == down => return Ok(()),
            SynthButton::Left => self.left_down = down,
            SynthButton::Right => self.right_down = down,
            SynthButton::Middle => self.middle_down = down,
        }
        let (kind, cg) = match (button, down) {
            (SynthButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (SynthButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (SynthButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (SynthButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (SynthButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (SynthButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };
        self.post(kind, self.last, cg)
    }

    fn post(&self, kind: CGEventType, point: CGPoint, button: CGMouseButton) -> Result<()> {
        let event = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
            .map_err(|()| anyhow!("could not build a Core Graphics mouse event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        // Never leave the game thinking a button is held because the session ended mid-shot.
        if self.left_down {
            let _ = self.post(CGEventType::LeftMouseUp, self.last, CGMouseButton::Left);
        }
        if self.right_down {
            let _ = self.post(CGEventType::RightMouseUp, self.last, CGMouseButton::Right);
        }
        if self.middle_down {
            let _ = self.post(CGEventType::OtherMouseUp, self.last, CGMouseButton::Center);
        }
    }
}
