//! V4L2 capture with explicit buffer control.
//!
//! The one behaviour that matters for latency lives in [`Stream::next`]: after blocking for
//! a frame, it drains any frames that are already waiting and hands back only the newest,
//! so a hiccup can never leave the driver permanently 3-4 frames behind.

// Raw ioctls and mmap need `unsafe`; it is confined to this module and `sys`.
#![allow(unsafe_code)]

pub mod sys;

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, warn};

use super::{cid, ControlDesc, ControlType, FormatDesc, Frame, FrameSize, PixelFormat};

fn cstr_bytes(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// Monotonic clock as a `Duration`, comparable with V4L2 monotonic timestamps.
pub fn monotonic_now() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: timespec is a plain C struct and CLOCK_MONOTONIC is always valid.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // tv_nsec < 1e9
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

#[derive(Clone, Debug)]
pub struct Capability {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
}

impl Capability {
    pub fn is_video_capture(&self) -> bool {
        let caps = if self.capabilities & sys::V4L2_CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        };
        caps & sys::V4L2_CAP_VIDEO_CAPTURE != 0 && caps & sys::V4L2_CAP_STREAMING != 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub bytes_per_line: u32,
    pub size_image: u32,
}

#[derive(Debug)]
pub struct Device {
    fd: OwnedFd,
    path: PathBuf,
}

impl Device {
    pub fn open(path: &Path) -> io::Result<Self> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: c is a valid NUL-terminated string; flags are plain constants.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a freshly opened descriptor we own.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Issue an ioctl with a pointer argument, retrying on EINTR.
    ///
    /// # Safety
    /// `arg` must point to a properly sized struct for `req`.
    unsafe fn ioctl<T>(&self, req: u32, arg: *mut T) -> io::Result<()> {
        loop {
            #[allow(clippy::cast_lossless)]
            let r = libc::ioctl(self.fd.as_raw_fd(), req as _, arg);
            if r >= 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    pub fn capability(&self) -> io::Result<Capability> {
        // SAFETY: zeroed v4l2_capability is a valid value; the kernel fills it.
        let mut cap: sys::v4l2_capability = unsafe { std::mem::zeroed() };
        unsafe { self.ioctl(sys::VIDIOC_QUERYCAP, &mut cap)? };
        Ok(Capability {
            driver: cstr_bytes(&cap.driver),
            card: cstr_bytes(&cap.card),
            bus_info: cstr_bytes(&cap.bus_info),
            version: cap.version,
            capabilities: cap.capabilities,
            device_caps: cap.device_caps,
        })
    }

    /// Enumerate pixel formats, frame sizes and frame intervals.
    pub fn formats(&self) -> io::Result<Vec<FormatDesc>> {
        let mut out = Vec::new();
        for index in 0.. {
            // SAFETY: zeroed struct is valid input; kernel fills it.
            let mut d: sys::v4l2_fmtdesc = unsafe { std::mem::zeroed() };
            d.index = index;
            d.type_ = sys::V4L2_BUF_TYPE_VIDEO_CAPTURE;
            match unsafe { self.ioctl(sys::VIDIOC_ENUM_FMT, &mut d) } {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
            let mut sizes = Vec::new();
            for si in 0.. {
                let mut s: sys::v4l2_frmsizeenum = unsafe { std::mem::zeroed() };
                s.index = si;
                s.pixel_format = d.pixelformat;
                match unsafe { self.ioctl(sys::VIDIOC_ENUM_FRAMESIZES, &mut s) } {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                    Err(e) => return Err(e),
                }
                if s.type_ != sys::V4L2_FRMSIZE_TYPE_DISCRETE {
                    break;
                }
                // SAFETY: type_ says the discrete member is the active one.
                let (w, h) = unsafe { (s.u.discrete.width, s.u.discrete.height) };
                let mut intervals = Vec::new();
                for ii in 0.. {
                    let mut iv: sys::v4l2_frmivalenum = unsafe { std::mem::zeroed() };
                    iv.index = ii;
                    iv.pixel_format = d.pixelformat;
                    iv.width = w;
                    iv.height = h;
                    match unsafe { self.ioctl(sys::VIDIOC_ENUM_FRAMEINTERVALS, &mut iv) } {
                        Ok(()) => {}
                        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                        Err(e) => return Err(e),
                    }
                    if iv.type_ != sys::V4L2_FRMIVAL_TYPE_DISCRETE {
                        break;
                    }
                    let f = unsafe { iv.u.discrete };
                    if !intervals.contains(&(f.numerator, f.denominator)) {
                        intervals.push((f.numerator, f.denominator));
                    }
                }
                sizes.push(FrameSize {
                    width: w,
                    height: h,
                    intervals,
                });
            }
            out.push(FormatDesc {
                format: PixelFormat::from_fourcc(d.pixelformat),
                description: cstr_bytes(&d.description),
                sizes,
            });
        }
        Ok(out)
    }

    /// Enumerate every control the device exposes, with current values.
    pub fn controls(&self) -> io::Result<Vec<ControlDesc>> {
        let mut out = Vec::new();
        let mut id = sys::V4L2_CTRL_FLAG_NEXT_CTRL;
        loop {
            let mut q: sys::v4l2_queryctrl = unsafe { std::mem::zeroed() };
            q.id = id;
            match unsafe { self.ioctl(sys::VIDIOC_QUERYCTRL, &mut q) } {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
            id = q.id | sys::V4L2_CTRL_FLAG_NEXT_CTRL;
            if q.flags & sys::V4L2_CTRL_FLAG_DISABLED != 0
                || q.type_ == sys::V4L2_CTRL_TYPE_CTRL_CLASS
            {
                continue;
            }
            let kind = match q.type_ {
                sys::V4L2_CTRL_TYPE_INTEGER => ControlType::Integer,
                sys::V4L2_CTRL_TYPE_BOOLEAN => ControlType::Boolean,
                sys::V4L2_CTRL_TYPE_MENU => ControlType::Menu,
                sys::V4L2_CTRL_TYPE_INTEGER_MENU => ControlType::IntegerMenu,
                other => ControlType::Other(other),
            };
            let mut menu = Vec::new();
            if matches!(kind, ControlType::Menu | ControlType::IntegerMenu) {
                for mi in q.minimum..=q.maximum {
                    let mut m: sys::v4l2_querymenu = unsafe { std::mem::zeroed() };
                    m.id = q.id;
                    #[allow(clippy::cast_sign_loss)]
                    {
                        m.index = mi as u32;
                    }
                    if unsafe { self.ioctl(sys::VIDIOC_QUERYMENU, &mut m) }.is_ok() {
                        let label = if kind == ControlType::Menu {
                            let name = m.name;
                            cstr_bytes(&name)
                        } else {
                            let name = m.name;
                            i64::from_le_bytes(name[..8].try_into().unwrap_or([0; 8])).to_string()
                        };
                        menu.push((mi, label));
                    }
                }
            }
            let value = self.get_control(q.id).ok();
            out.push(ControlDesc {
                id: q.id,
                name: cstr_bytes(&q.name),
                kind,
                min: q.minimum,
                max: q.maximum,
                step: q.step,
                default: q.default_value,
                value,
                inactive: q.flags & sys::V4L2_CTRL_FLAG_INACTIVE != 0,
                menu,
            });
        }
        Ok(out)
    }

    pub fn get_control(&self, id: u32) -> io::Result<i32> {
        let mut c = sys::v4l2_control { id, value: 0 };
        unsafe { self.ioctl(sys::VIDIOC_G_CTRL, &mut c)? };
        Ok(c.value)
    }

    pub fn set_control(&self, id: u32, value: i32) -> io::Result<()> {
        let mut c = sys::v4l2_control { id, value };
        unsafe { self.ioctl(sys::VIDIOC_S_CTRL, &mut c) }
    }

    /// Set manual exposure in 100 µs units (`V4L2_CID_EXPOSURE_ABSOLUTE`).
    pub fn set_manual_exposure(&self, units_100us: i32) -> io::Result<()> {
        self.set_control(cid::EXPOSURE_AUTO, cid::EXPOSURE_MANUAL)?;
        self.set_control(cid::EXPOSURE_ABSOLUTE, units_100us)
    }

    pub fn set_auto_exposure(&self) -> io::Result<()> {
        self.set_control(cid::EXPOSURE_AUTO, cid::EXPOSURE_APERTURE_PRIORITY)
    }

    pub fn set_format(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> io::Result<PixFormat> {
        let mut f = sys::v4l2_format {
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            fmt: sys::v4l2_format_union { raw_data: [0; 200] },
        };
        f.fmt.pix = sys::v4l2_pix_format {
            width,
            height,
            pixelformat: format.fourcc(),
            field: sys::V4L2_FIELD_NONE,
            ..Default::default()
        };
        unsafe { self.ioctl(sys::VIDIOC_S_FMT, &mut f)? };
        let pix = unsafe { f.fmt.pix };
        let got = PixFormat {
            width: pix.width,
            height: pix.height,
            format: PixelFormat::from_fourcc(pix.pixelformat),
            bytes_per_line: pix.bytesperline,
            size_image: pix.sizeimage,
        };
        if got.width != width || got.height != height || got.format != format {
            warn!(?got, "driver adjusted the requested format");
        }
        Ok(got)
    }

    pub fn get_format(&self) -> io::Result<PixFormat> {
        let mut f = sys::v4l2_format {
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            fmt: sys::v4l2_format_union { raw_data: [0; 200] },
        };
        unsafe { self.ioctl(sys::VIDIOC_G_FMT, &mut f)? };
        let pix = unsafe { f.fmt.pix };
        Ok(PixFormat {
            width: pix.width,
            height: pix.height,
            format: PixelFormat::from_fourcc(pix.pixelformat),
            bytes_per_line: pix.bytesperline,
            size_image: pix.sizeimage,
        })
    }

    /// Request a frame interval (seconds = numerator/denominator). Returns what the driver chose.
    pub fn set_frame_interval(&self, numerator: u32, denominator: u32) -> io::Result<(u32, u32)> {
        let mut p = sys::v4l2_streamparm {
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            parm: sys::v4l2_streamparm_union { raw_data: [0; 200] },
        };
        p.parm.capture = sys::v4l2_captureparm {
            timeperframe: sys::v4l2_fract {
                numerator,
                denominator,
            },
            ..Default::default()
        };
        unsafe { self.ioctl(sys::VIDIOC_S_PARM, &mut p)? };
        let tpf = unsafe { p.parm.capture.timeperframe };
        Ok((tpf.numerator, tpf.denominator))
    }

    pub fn get_frame_interval(&self) -> io::Result<(u32, u32)> {
        let mut p = sys::v4l2_streamparm {
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            parm: sys::v4l2_streamparm_union { raw_data: [0; 200] },
        };
        unsafe { self.ioctl(sys::VIDIOC_G_PARM, &mut p)? };
        let tpf = unsafe { p.parm.capture.timeperframe };
        Ok((tpf.numerator, tpf.denominator))
    }

    /// Map `count` buffers and start streaming.
    pub fn start_stream(&self, count: u32) -> io::Result<Stream<'_>> {
        let mut req = sys::v4l2_requestbuffers {
            count,
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: sys::V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        unsafe { self.ioctl(sys::VIDIOC_REQBUFS, &mut req)? };
        if req.count == 0 {
            return Err(io::Error::other("driver granted zero buffers"));
        }
        let mut bufs = Vec::with_capacity(req.count as usize);
        for index in 0..req.count {
            let mut b = sys::v4l2_buffer::zeroed_capture(index);
            unsafe { self.ioctl(sys::VIDIOC_QUERYBUF, &mut b)? };
            let len = b.length as usize;
            let offset = unsafe { b.m.offset };
            // SAFETY: mapping a V4L2 buffer at the offset the driver reported.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    self.fd.as_raw_fd(),
                    libc::off_t::from(offset),
                )
            };
            if ptr == libc::MAP_FAILED {
                let e = io::Error::last_os_error();
                for m in &bufs {
                    let Mapped { ptr, len } = *m;
                    unsafe { libc::munmap(ptr, len) };
                }
                return Err(e);
            }
            bufs.push(Mapped { ptr, len });
        }
        let stream = Stream {
            dev: self,
            bufs,
            held: None,
            timestamp_flags_seen: None,
        };
        for index in 0..req.count {
            stream.queue(index)?;
        }
        let mut ty = sys::V4L2_BUF_TYPE_VIDEO_CAPTURE.cast_signed();
        unsafe { self.ioctl(sys::VIDIOC_STREAMON, &mut ty)? };
        debug!(count = req.count, "stream started");
        Ok(stream)
    }
}

#[derive(Clone, Copy, Debug)]
struct Mapped {
    ptr: *mut libc::c_void,
    len: usize,
}

/// A running capture stream. Dropping it stops streaming and unmaps the buffers.
pub struct Stream<'a> {
    dev: &'a Device,
    bufs: Vec<Mapped>,
    held: Option<u32>,
    timestamp_flags_seen: Option<u32>,
}

impl Stream<'_> {
    fn queue(&self, index: u32) -> io::Result<()> {
        let mut b = sys::v4l2_buffer::zeroed_capture(index);
        unsafe { self.dev.ioctl(sys::VIDIOC_QBUF, &mut b) }
    }

    fn dequeue(&self) -> io::Result<sys::v4l2_buffer> {
        let mut b = sys::v4l2_buffer::zeroed_capture(0);
        unsafe { self.dev.ioctl(sys::VIDIOC_DQBUF, &mut b)? };
        Ok(b)
    }

    /// Wait until a frame is ready. `None` timeout blocks indefinitely. Returns false on timeout.
    fn wait_ready(&self, timeout: Option<Duration>) -> io::Result<bool> {
        let mut pfd = libc::pollfd {
            fd: self.dev.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = match timeout {
            None => -1,
            Some(t) => i32::try_from(t.as_millis()).unwrap_or(i32::MAX),
        };
        loop {
            // SAFETY: pfd is a valid pollfd array of length 1.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r > 0 {
                if pfd.revents & libc::POLLERR != 0 {
                    return Err(io::Error::other("poll reported POLLERR on capture device"));
                }
                return Ok(true);
            }
            if r == 0 {
                return Ok(false);
            }
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    /// Timestamp flags of the last dequeued buffer (`V4L2_BUF_FLAG_TIMESTAMP_*`).
    pub fn timestamp_source(&self) -> Option<&'static str> {
        self.timestamp_flags_seen
            .map(|f| match f & sys::V4L2_BUF_FLAG_TIMESTAMP_MASK {
                sys::V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC => "monotonic",
                sys::V4L2_BUF_FLAG_TIMESTAMP_COPY => "copy",
                _ => "unknown",
            })
    }

    /// Get the next frame. With `newest`, drain everything already queued and return only the
    /// most recent frame, counting the rest as dropped. Returns `None` on timeout.
    ///
    /// The returned frame borrows a driver buffer; it is requeued on the next call.
    pub fn next(
        &mut self,
        timeout: Option<Duration>,
        newest: bool,
    ) -> io::Result<Option<Frame<'_>>> {
        if let Some(idx) = self.held.take() {
            self.queue(idx)?;
        }
        if !self.wait_ready(timeout)? {
            return Ok(None);
        }
        let mut buf = self.dequeue()?;
        let mut dropped = 0;
        if newest {
            while self.wait_ready(Some(Duration::ZERO))? {
                let newer = self.dequeue()?;
                self.queue(buf.index)?;
                buf = newer;
                dropped += 1;
            }
        }
        let dequeued_at = monotonic_now();
        self.timestamp_flags_seen = Some(buf.flags);
        let timestamp = if buf.flags & sys::V4L2_BUF_FLAG_TIMESTAMP_MASK
            == sys::V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC
        {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // tv_usec < 1e6
            Some(Duration::new(
                buf.timestamp.tv_sec as u64,
                (buf.timestamp.tv_usec as u32).saturating_mul(1000),
            ))
        } else {
            None
        };
        self.held = Some(buf.index);
        let m = self.bufs[buf.index as usize];
        let used = (buf.bytesused as usize).min(m.len);
        // SAFETY: the buffer is mapped for m.len bytes and stays mapped while the stream lives;
        // the driver owns no other reference to it while it is dequeued.
        let data = unsafe { std::slice::from_raw_parts(m.ptr.cast::<u8>(), used) };
        Ok(Some(Frame {
            data,
            sequence: buf.sequence,
            timestamp,
            dequeued_at,
            dropped,
            error: buf.flags & sys::V4L2_BUF_FLAG_ERROR != 0,
        }))
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        let mut ty = sys::V4L2_BUF_TYPE_VIDEO_CAPTURE.cast_signed();
        // Errors here are unrecoverable and uninteresting; the fd closes with the Device.
        let _ = unsafe { self.dev.ioctl(sys::VIDIOC_STREAMOFF, &mut ty) };
        for m in &self.bufs {
            unsafe { libc::munmap(m.ptr, m.len) };
        }
        // Release the buffers so a later REQBUFS on the same device works.
        let mut req = sys::v4l2_requestbuffers {
            count: 0,
            type_: sys::V4L2_BUF_TYPE_VIDEO_CAPTURE,
            memory: sys::V4L2_MEMORY_MMAP,
            ..Default::default()
        };
        let _ = unsafe { self.dev.ioctl(sys::VIDIOC_REQBUFS, &mut req) };
    }
}

/// Name of a control class prefix, for display.
pub fn control_name(id: u32) -> Option<&'static str> {
    Some(match id {
        cid::BRIGHTNESS => "brightness",
        cid::CONTRAST => "contrast",
        cid::SATURATION => "saturation",
        cid::HUE => "hue",
        cid::AUTO_WHITE_BALANCE => "white_balance_automatic",
        cid::GAMMA => "gamma",
        cid::GAIN => "gain",
        cid::POWER_LINE_FREQUENCY => "power_line_frequency",
        cid::WHITE_BALANCE_TEMPERATURE => "white_balance_temperature",
        cid::SHARPNESS => "sharpness",
        cid::BACKLIGHT_COMPENSATION => "backlight_compensation",
        cid::EXPOSURE_AUTO => "auto_exposure",
        cid::EXPOSURE_ABSOLUTE => "exposure_time_absolute",
        cid::ZOOM_ABSOLUTE => "zoom_absolute",
        _ => return None,
    })
}
