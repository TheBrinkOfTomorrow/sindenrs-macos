//! What the overlay shows, written by the tracker and read by the window.

use std::time::Instant;

/// Whether the camera can currently see the whole border.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// The whole border is in view; measurements are trustworthy.
    Good,
    /// The border runs off the edge of the camera frame, so the solve is unreliable.
    Clipped,
    /// No border found at all.
    #[default]
    Lost,
}

/// Everything the overlay draws. Updated by the worker, read by the event loop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    /// Border thickness as a fraction of the shorter screen dimension.
    pub border_frac: f64,
    /// Calibration targets in screen percent; empty draws just the border.
    pub targets: Vec<[f64; 2]>,
    /// Which target is being measured.
    pub current: Option<usize>,
    /// Where each measured target actually read.
    pub measured: Vec<Option<[f64; 2]>>,
    /// Live aim point in screen percent.
    pub aim: Option<[f64; 2]>,
    pub quality: Quality,
    /// What the last solve rested on.
    pub solve: SolveInfo,
    /// Set to flash the current target when a shot could not be measured.
    pub flash_until: Option<Instant>,
    pub done: bool,
    /// Draw nothing at all (the window stays, fully transparent): the border switched off
    /// from the macOS menu bar, e.g. while the game or MAME artwork draws its own.
    pub hidden: bool,
}

/// What the tracker's last solve was built from, shown so the operator can tell a
/// four-edge solve from a two-edge-plus-tabs one and see where the camera is looking.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SolveInfo {
    /// Sides with a fitted outer edge, one bit per `vision::code::Side` index.
    pub sides: u8,
    /// Decoded tabs used.
    pub tabs: u8,
    /// True if the corners came from edge lines (false: hull, unreliable).
    pub from_lines: bool,
    /// The camera frame's corners in screen percent, if solved.
    pub view: Option<[[f64; 2]; 4]>,
}

impl Scene {
    pub fn border_only(border_frac: f64) -> Self {
        Self {
            border_frac,
            ..Default::default()
        }
    }
}
