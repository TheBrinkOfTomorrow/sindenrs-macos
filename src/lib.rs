//! sindenrs: a clean-room driver for the Sinden Lightgun.
//!
//! The crate is split along the lines of the redesign notes:
//!
//! - [`protocol`] — the wire protocol (pure, platform independent, unit tested).
//! - [`gun`] — a serial session with one gun: authentication, setup, position writes, events.
//! - [`discovery`] — finding guns and cameras (sysfs on Linux; stubbed elsewhere).
//! - [`camera`] — frame capture (V4L2 on Linux; stubbed elsewhere).
//! - [`vision`] — image geometry and pixel helpers.
//!
//! The pointer itself is reported by the gun's own HID mouse, so nothing here touches
//! the display server. Wayland, X11 and Windows only matter for the (future) border
//! overlay, which will live behind its own module.

pub mod camera;
pub mod config;
pub mod discovery;
pub mod firmware;
pub mod gun;
pub mod ids;
pub mod overlay;
pub mod protocol;
pub mod runtime;
pub mod usb;
pub mod vision;
