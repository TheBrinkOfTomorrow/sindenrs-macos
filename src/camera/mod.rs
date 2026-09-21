//! Frame capture.
//!
//! Platform-neutral types live here; the Linux implementation is [`v4l2`]. Windows is a stub
//! until a Media Foundation backend exists.

use std::fmt;
use std::time::Duration;

#[cfg(target_os = "linux")]
pub mod v4l2;

#[cfg(not(target_os = "linux"))]
pub mod v4l2 {
    //! Stub so the rest of the crate compiles off Linux.
    #[derive(Debug)]
    pub struct Device;
    impl Device {
        pub fn open(_path: &std::path::Path) -> std::io::Result<Self> {
            Err(std::io::Error::other(
                "camera capture is not implemented on this platform yet",
            ))
        }
    }
}

/// Pixel formats the driver knows how to consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Motion-JPEG; one JPEG per frame. 60 fps at 640x480 on the Sinden camera.
    Mjpeg,
    /// Packed YUV 4:2:2; luma is every other byte. 30 fps at 640x480 on the Sinden camera.
    Yuyv,
    Other(u32),
}

impl PixelFormat {
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Mjpeg => fourcc(b"MJPG"),
            Self::Yuyv => fourcc(b"YUYV"),
            Self::Other(f) => f,
        }
    }

    pub const fn from_fourcc(f: u32) -> Self {
        if f == fourcc(b"MJPG") {
            Self::Mjpeg
        } else if f == fourcc(b"YUYV") {
            Self::Yuyv
        } else {
            Self::Other(f)
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.fourcc().to_le_bytes();
        write!(f, "{}", String::from_utf8_lossy(&b))
    }
}

pub const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// A frame size and the intervals the device offers for it.
#[derive(Clone, Debug)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
    /// Frame intervals as (numerator, denominator) seconds.
    pub intervals: Vec<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub struct FormatDesc {
    pub format: PixelFormat,
    pub description: String,
    pub sizes: Vec<FrameSize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlType {
    Integer,
    Boolean,
    Menu,
    IntegerMenu,
    Other(u32),
}

#[derive(Clone, Debug)]
pub struct ControlDesc {
    pub id: u32,
    pub name: String,
    pub kind: ControlType,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    pub value: Option<i32>,
    pub inactive: bool,
    /// Menu entries as (index, label).
    pub menu: Vec<(i32, String)>,
}

/// Standard control IDs we care about (`V4L2_CID_*`; the same numbers UVC exposes).
pub mod cid {
    pub const BRIGHTNESS: u32 = 0x0098_0900;
    pub const CONTRAST: u32 = 0x0098_0901;
    pub const SATURATION: u32 = 0x0098_0902;
    pub const HUE: u32 = 0x0098_0903;
    pub const AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
    pub const GAMMA: u32 = 0x0098_0910;
    pub const GAIN: u32 = 0x0098_0913;
    pub const POWER_LINE_FREQUENCY: u32 = 0x0098_0918;
    pub const WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;
    pub const EXPOSURE_AUTO: u32 = 0x009a_0901;
    pub const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    pub const ZOOM_ABSOLUTE: u32 = 0x009a_090d;

    /// `V4L2_CID_EXPOSURE_AUTO` menu values.
    pub const EXPOSURE_MANUAL: i32 = 1;
    pub const EXPOSURE_APERTURE_PRIORITY: i32 = 3;
}

/// One captured frame, borrowed from the driver's buffer until the next dequeue.
#[derive(Debug)]
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub sequence: u32,
    /// Capture timestamp on the monotonic clock, if the driver provided one.
    pub timestamp: Option<Duration>,
    /// Monotonic time at which the dequeue returned.
    pub dequeued_at: Duration,
    /// Frames discarded to reach this one (newest-frame drain).
    pub dropped: u32,
    /// Driver flagged this frame as corrupt.
    pub error: bool,
}

impl Frame<'_> {
    /// How old the frame was when we got it, if a timestamp is available.
    pub fn age(&self) -> Option<Duration> {
        self.timestamp.map(|ts| self.dequeued_at.saturating_sub(ts))
    }
}
