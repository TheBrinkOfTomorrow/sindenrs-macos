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
//! The pointer is reported by the gun's own HID mouse on Linux and Windows, so nothing there
//! touches the display server. macOS ignores an absolute HID pointer, so on that platform the
//! driver moves the system cursor itself via [`cursor`]. Wayland, X11 and the macOS overlay
//! draw the border the gun tracks.

pub mod activity;
pub mod camera;
pub mod config;
pub mod control;
#[cfg(target_os = "macos")]
pub mod cursor;
pub mod discovery;
pub mod firmware;
pub mod gun;
pub mod ids;
#[cfg(target_os = "macos")]
pub mod menubar;
pub mod overlay;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod preview;
pub mod protocol;
pub mod runtime;
pub mod usb;
pub mod vision;
