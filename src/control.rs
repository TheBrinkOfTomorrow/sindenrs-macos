//! A request the UI (menu bar) leaves for `main` to act on after `run` winds down.
//!
//! The menu runs inside `run`'s process but cannot calibrate or reload in place: both need
//! the guns and camera released first. So a menu item records a request here, sets the stop
//! flag, and `main`'s run loop reads the request once `run` has shut down, acts on it, and
//! goes back into `run`.

use std::sync::atomic::{AtomicU8, Ordering};

/// What should happen once `run` has wound down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Request {
    /// Nothing: the stop was a real quit.
    #[default]
    None = 0,
    /// Run the calibration flow, then return to `run`.
    Calibrate = 1,
    /// Reload the configuration file and return to `run`.
    Reload = 2,
}

/// The slot a request is left in. One per process run loop.
#[derive(Debug, Default)]
pub struct Control(AtomicU8);

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, r: Request) {
        self.0.store(r as u8, Ordering::Relaxed);
    }

    /// The pending request, cleared by reading it.
    pub fn take(&self) -> Request {
        match self.0.swap(Request::None as u8, Ordering::Relaxed) {
            1 => Request::Calibrate,
            2 => Request::Reload,
            _ => Request::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_clears() {
        let c = Control::new();
        assert_eq!(c.take(), Request::None);
        c.set(Request::Calibrate);
        assert_eq!(c.take(), Request::Calibrate);
        assert_eq!(c.take(), Request::None);
        c.set(Request::Reload);
        c.set(Request::Calibrate);
        assert_eq!(c.take(), Request::Calibrate, "last request wins");
    }
}
