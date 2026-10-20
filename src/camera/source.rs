//! The camera as the tracker sees it: opened with a display profile's settings, yielding luma
//! frames. Each platform keeps its own capture path behind this:
//!
//! - Linux: V4L2 MJPEG, decoded here, with truncated frames (a failing USB link) detected by
//!   their missing end-of-image marker (and, as a backstop, by size against the frames around
//!   them) and reported as [`Next::Corrupt`].
//! - macOS: AVFoundation delivers decoded luma; the UVC controls go over IOKit.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::Display;

/// The frame size the tracker runs at.
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;

/// One frame, ready for the vision code.
pub struct LumaFrame<'a> {
    /// Packed `width * height` luma; the tracker flips it in place.
    pub luma: &'a mut [u8],
    /// The frame as captured, for saving: MJPEG on Linux, a binary PGM of the unflipped luma
    /// on macOS. [`Self::raw_ext`] is its file extension.
    pub raw: &'a [u8],
    pub raw_ext: &'static str,
    pub sequence: u32,
    /// Capture time, on the clock [`Self::age`] is measured against.
    pub timestamp: Option<Duration>,
    pub age: Option<Duration>,
    /// Time spent decoding the frame (zero where the platform delivers luma).
    pub decode: Duration,
}

/// What [`Frames::next`] got.
pub enum Next<'a> {
    Frame(LumaFrame<'a>),
    /// Nothing within the timeout.
    Timeout,
    /// A frame arrived but could not be used (truncated or undecodable).
    Corrupt,
}

/// Whether a JPEG ends with its end-of-image marker (`FF D9`), ignoring any zero padding a
/// camera appends. A frame the USB link truncated has lost its end; non-strict decoding would
/// fill the missing part with flat grey, which the solver reads as a bright screen.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn has_eoi(data: &[u8]) -> bool {
    let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    end >= 2 && data[end - 2] == 0xFF && data[end - 1] == 0xD9
}

/// Whether a frame is far smaller than the recent ones: a truncated frame whose end happened
/// to survive. `sizes` must hold every recent frame, accepted or not: a legitimate frame is
/// anywhere from 7 KB (the gun put down facing a dark floor) to 60 KB (the lit screen), so a
/// window of accepted sizes alone can rise and then reject every later frame for good.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn size_outlier(size: usize, sizes: &std::collections::VecDeque<usize>) -> bool {
    if sizes.len() < 10 {
        return false;
    }
    let mut v: Vec<usize> = sizes.iter().copied().collect();
    v.sort_unstable();
    size * 10 < v[v.len() / 2] * 6
}

/// Apply a display profile's camera controls through the `set_manual_exposure` /
/// `set_auto_exposure` / `set_control` methods both backends offer.
macro_rules! apply_display {
    ($ctl:expr, $d:expr) => {{
        use $crate::camera::cid;
        let (ctl, d): (_, &$crate::config::Display) = ($ctl, $d);
        match d.exposure {
            $crate::config::Exposure::Manual(v) => ctl.set_manual_exposure(v)?,
            $crate::config::Exposure::Auto(_) => ctl.set_auto_exposure()?,
        }
        for (id, val, name) in [
            (cid::BRIGHTNESS, d.brightness, "brightness"),
            (cid::CONTRAST, d.contrast, "contrast"),
            (cid::GAIN, d.gain, "gain"),
            (cid::GAMMA, d.gamma, "gamma"),
            (cid::SHARPNESS, d.sharpness, "sharpness"),
        ] {
            if let Some(v) = val {
                anyhow::Context::with_context(ctl.set_control(id, v), || {
                    format!("setting {name}={v}")
                })?;
            }
        }
    }};
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::VecDeque;

    use tracing::{debug, info};

    use super::{
        has_eoi, size_outlier, Context, Display, Duration, LumaFrame, Next, Path, Result, HEIGHT,
        WIDTH,
    };
    use crate::camera::v4l2::{Device, Stream};
    use crate::camera::PixelFormat;
    use crate::vision::luma;

    pub struct Camera {
        dev: Device,
        width: usize,
        height: usize,
        description: String,
    }

    /// Name the usual cause of EBUSY: another process streaming the same camera.
    fn busy(e: std::io::Error, camera: &Path) -> anyhow::Error {
        if e.raw_os_error() == Some(16) {
            anyhow::Error::new(e).context(format!(
                "{} is busy: another process is streaming it (a `track`, `run` or \
                 `aim-test` still going?); only one can use a camera at a time",
                camera.display()
            ))
        } else {
            e.into()
        }
    }

    impl Camera {
        pub fn open(camera: &Path, d: &Display) -> Result<Self> {
            let dev =
                Device::open(camera).with_context(|| format!("opening {}", camera.display()))?;
            let fmt = dev
                .set_format(WIDTH, HEIGHT, PixelFormat::Mjpeg)
                .map_err(|e| busy(e, camera))?;
            if let Some(fps) = d.fps {
                let (n, den) = dev.set_frame_interval(1, fps)?;
                info!("frame interval {n}/{den} s");
            }
            apply_display!(&dev, d);
            Ok(Self {
                dev,
                width: fmt.width as usize,
                height: fmt.height as usize,
                description: format!("{}x{} {}", fmt.width, fmt.height, fmt.format),
            })
        }

        pub const fn size(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        pub fn description(&self) -> &str {
            &self.description
        }

        pub fn start(&self, buffers: u32, camera: &Path) -> Result<Frames<'_>> {
            let stream = self
                .dev
                .start_stream(buffers)
                .map_err(|e| busy(e, camera))?;
            Ok(Frames {
                stream,
                sizes: VecDeque::new(),
                luma: Vec::new(),
                pixels: self.width * self.height,
            })
        }
    }

    pub struct Frames<'a> {
        stream: Stream<'a>,
        /// Recent frame sizes, accepted or not, for spotting truncated frames.
        sizes: VecDeque<usize>,
        luma: Vec<u8>,
        pixels: usize,
    }

    impl Frames<'_> {
        pub fn next(&mut self, timeout: Duration) -> Result<Next<'_>> {
            let Some(frame) = self.stream.next(Some(timeout), true)? else {
                return Ok(Next::Timeout);
            };
            // A frame the USB link truncated has lost its end-of-image marker; the size check
            // is a backstop for one that kept it. Every frame's size goes into the window, so
            // a scene that turns plain (the gun put down) is followed, not rejected for good.
            let size = frame.data.len();
            let outlier = size_outlier(size, &self.sizes);
            self.sizes.push_back(size);
            if self.sizes.len() > 30 {
                self.sizes.pop_front();
            }
            if frame.error || !has_eoi(frame.data) || outlier {
                debug!(
                    "corrupt frame {}: {} bytes, driver error {}, end marker {}, size outlier {}",
                    frame.sequence,
                    size,
                    frame.error,
                    has_eoi(frame.data),
                    outlier
                );
                return Ok(Next::Corrupt);
            }
            let t0 = std::time::Instant::now();
            let Ok((_, _, l)) = luma::mjpeg_to_luma(frame.data) else {
                debug!("corrupt frame {}: {size} bytes, undecodable", frame.sequence);
                return Ok(Next::Corrupt);
            };
            if l.len() != self.pixels {
                debug!("corrupt frame {}: decoded to {} pixels", frame.sequence, l.len());
                return Ok(Next::Corrupt);
            }
            self.luma = l;
            Ok(Next::Frame(LumaFrame {
                luma: &mut self.luma,
                raw: frame.data,
                raw_ext: "jpg",
                sequence: frame.sequence,
                timestamp: frame.timestamp,
                age: frame.age(),
                decode: t0.elapsed(),
            }))
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io::Write as _;
    use std::marker::PhantomData;

    use super::{Context, Display, Duration, LumaFrame, Next, Path, Result, HEIGHT, WIDTH};
    use crate::camera::avfoundation::{Device, Stream};
    use crate::camera::uvc::iokit::Controls;

    pub struct Camera {
        dev: Device,
        width: usize,
        height: usize,
        description: String,
    }

    impl Camera {
        /// Open `camera` (an AVFoundation `uniqueID`) and apply `d`'s controls. The frame rate
        /// is the camera's fastest for the size; `d.fps` does not apply.
        pub fn open(camera: &Path, d: &Display) -> Result<Self> {
            let ctl = Controls::open(camera)
                .with_context(|| format!("opening camera controls of {}", camera.display()))?;
            apply_display!(&ctl, d);
            let dev = Device::open(camera)
                .with_context(|| format!("opening camera {}", camera.display()))?;
            Ok(Self {
                dev,
                width: WIDTH as usize,
                height: HEIGHT as usize,
                description: format!("{WIDTH}x{HEIGHT} 420v"),
            })
        }

        pub const fn size(&self) -> (usize, usize) {
            (self.width, self.height)
        }

        pub fn description(&self) -> &str {
            &self.description
        }

        pub fn start(&self, _buffers: u32, _camera: &Path) -> Result<Frames<'_>> {
            Ok(Frames {
                stream: self.dev.start_stream(WIDTH, HEIGHT)?,
                pgm: Vec::new(),
                _camera: PhantomData,
            })
        }
    }

    pub struct Frames<'a> {
        stream: Stream,
        /// The current frame as PGM, for [`LumaFrame::raw`].
        pgm: Vec<u8>,
        _camera: PhantomData<&'a Camera>,
    }

    impl Frames<'_> {
        pub fn next(&mut self, timeout: Duration) -> Result<Next<'_>> {
            let (w, h) = (self.stream.width(), self.stream.height());
            let Some(f) = self.stream.next_luma(Some(timeout), true)? else {
                return Ok(Next::Timeout);
            };
            self.pgm.clear();
            let _ = write!(self.pgm, "P5\n{w} {h}\n255\n");
            self.pgm.extend_from_slice(f.luma);
            let age = f.age();
            Ok(Next::Frame(LumaFrame {
                luma: f.luma,
                raw: &self.pgm,
                raw_ext: "pgm",
                sequence: f.sequence,
                timestamp: f.timestamp,
                age,
                decode: Duration::ZERO,
            }))
        }
    }
}

pub use imp::{Camera, Frames};

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::{has_eoi, size_outlier};

    #[test]
    fn eoi_marker_with_and_without_padding() {
        assert!(has_eoi(&[0xFF, 0xD8, 1, 2, 0xFF, 0xD9]));
        assert!(has_eoi(&[0xFF, 0xD8, 1, 2, 0xFF, 0xD9, 0, 0, 0]));
        assert!(!has_eoi(&[0xFF, 0xD8, 1, 2, 0xFF]));
        assert!(!has_eoi(&[0xFF, 0xD8, 1, 2, 0xD9, 0xFF]));
        assert!(!has_eoi(&[]));
        assert!(!has_eoi(&[0, 0]));
    }

    #[test]
    fn size_outlier_needs_a_window_and_follows_a_scene_change() {
        let mut sizes = VecDeque::new();
        assert!(!size_outlier(100, &sizes));
        sizes.extend([45_000; 30]);
        assert!(size_outlier(20_000, &sizes));
        assert!(!size_outlier(30_000, &sizes));
        // The gun is put down facing the floor: small frames keep coming and, once the window
        // holds them, they are the scene, not outliers.
        for _ in 0..30 {
            sizes.push_back(7_000);
            sizes.pop_front();
        }
        assert!(!size_outlier(7_000, &sizes));
    }
}
