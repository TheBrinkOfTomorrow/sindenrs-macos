//! macOS capture through AVFoundation.
//!
//! The camera is opened by its AVFoundation `uniqueID` (what macOS discovery reports as the
//! camera's node). macOS decodes the camera's MJPEG itself, so we ask for bi-planar 4:2:0
//! (`420v`), whose first plane is exactly the luma the vision code consumes: 640x480 at 60 fps
//! on the Sinden camera. Frames are copied out of the capture callback into a small pool of
//! buffers and handed over a channel; [`Stream::next`] returns the luma as a packed
//! `width * height` slice in [`Frame::data`].
//!
//! Camera access needs the TCC camera grant, which macOS gives per app: run from Terminal, or
//! from an app bundle that declares `NSCameraUsageDescription` (`tools/macos/probe-app.sh`).

#![allow(unsafe_code)]

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Mutex;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceFormat,
    AVCaptureDeviceInput, AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMClock, CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use super::Frame;

/// Frames in flight between the capture callback and the consumer. Two is enough for
/// newest-frame draining; more only adds latency.
const POOL: usize = 3;

/// A frame as the capture callback hands it over.
#[derive(Debug)]
struct Captured {
    luma: Vec<u8>,
    sequence: u32,
    timestamp: Option<Duration>,
}

/// Shared with the capture callback, which runs on its own dispatch queue.
struct SinkIvars {
    width: usize,
    height: usize,
    frames: SyncSender<Captured>,
    /// Buffers the consumer is done with, for reuse.
    spare: Mutex<Receiver<Vec<u8>>>,
    sequence: AtomicU32,
    /// Frames dropped by AVFoundation (late) or by us (consumer behind, pool empty).
    dropped: AtomicU32,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SindenrsFrameSink"]
    #[ivars = SinkIvars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameSink {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.deliver(sample);
        }

        #[unsafe(method(captureOutput:didDropSampleBuffer:fromConnection:))]
        fn did_drop(
            &self,
            _output: &AVCaptureOutput,
            _sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.ivars().dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
);

impl FrameSink {
    fn new(ivars: SinkIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ivars);
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// Copy the luma plane out of the sample buffer and send it on.
    fn deliver(&self, sample: &CMSampleBuffer) {
        let iv = self.ivars();
        let sequence = iv.sequence.fetch_add(1, Ordering::Relaxed);
        // SAFETY: a valid sample buffer for the duration of the callback.
        let Some(pb) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        let mut luma = iv
            .spare
            .lock()
            .ok()
            .and_then(|r| r.try_recv().ok())
            .unwrap_or_default();
        let flags = CVPixelBufferLockFlags::ReadOnly;
        // SAFETY: a valid pixel buffer, unlocked below with the same flags.
        if unsafe { CVPixelBufferLockBaseAddress(&pb, flags) } != 0 {
            return;
        }
        let (w, h) = (
            CVPixelBufferGetWidthOfPlane(&pb, 0),
            CVPixelBufferGetHeightOfPlane(&pb, 0),
        );
        let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
        let base = CVPixelBufferGetBaseAddressOfPlane(&pb, 0).cast::<u8>();
        if w == iv.width && h == iv.height && !base.is_null() && stride >= w {
            luma.clear();
            luma.reserve(w * h);
            for y in 0..h {
                // SAFETY: plane 0 holds `h` rows of `stride` bytes while the buffer is locked.
                let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w) };
                luma.extend_from_slice(row);
            }
        }
        // SAFETY: matches the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(&pb, flags) };
        if luma.len() != iv.width * iv.height {
            return;
        }
        // SAFETY: a valid sample buffer.
        let timestamp = cm_duration(unsafe { sample.presentation_time_stamp() });
        let frame = Captured {
            luma,
            sequence,
            timestamp,
        };
        if iv.frames.try_send(frame).is_err() {
            iv.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A `CMTime` on the host clock as a `Duration`, if valid and non-negative.
fn cm_duration(t: CMTime) -> Option<Duration> {
    if t.timescale <= 0 || t.value < 0 {
        return None;
    }
    // SAFETY: a plain value conversion.
    Some(Duration::from_secs_f64(unsafe { t.seconds() }))
}

/// Now on the host clock, the clock AVFoundation stamps capture times with.
fn host_now() -> Duration {
    // SAFETY: the host time clock is always available.
    cm_duration(unsafe { CMClock::host_time_clock().time() }).unwrap_or_default()
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Ask for, or check, the camera grant. Blocks while macOS shows its prompt.
fn ensure_access() -> io::Result<()> {
    // SAFETY: a framework constant.
    let video = unsafe { AVMediaTypeVideo }.ok_or_else(|| err("AVMediaTypeVideo missing"))?;
    // SAFETY: a valid media type.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(video) };
    let granted = if status == AVAuthorizationStatus::Authorized {
        true
    } else if status == AVAuthorizationStatus::NotDetermined {
        let (tx, rx) = mpsc::channel();
        let handler = block2::RcBlock::new(move |ok: Bool| {
            let _ = tx.send(ok.as_bool());
        });
        // SAFETY: a valid media type and a block that outlives the call (it is retained).
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video, &handler) };
        rx.recv().unwrap_or(false)
    } else {
        false
    };
    if granted {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "camera access denied: allow it under System Settings > Privacy & Security > Camera \
             for the app running sindenrs (Terminal, or the sindenrs app bundle); an app without \
             NSCameraUsageDescription, such as an editor or agent host, is refused without a prompt",
        ))
    }
}

/// An AVFoundation capture device, before streaming.
#[derive(Debug)]
pub struct Device {
    device: Retained<AVCaptureDevice>,
    unique_id: String,
}

// SAFETY: AVCaptureDevice is documented as usable from any thread.
unsafe impl Send for Device {}

impl Device {
    /// Open the camera whose AVFoundation `uniqueID` is `unique_id` (the camera node that
    /// discovery reports).
    pub fn open(unique_id: &Path) -> io::Result<Self> {
        ensure_access()?;
        let id = unique_id.to_string_lossy().into_owned();
        // SAFETY: a valid string.
        let device = unsafe { AVCaptureDevice::deviceWithUniqueID(&NSString::from_str(&id)) }
            .ok_or_else(|| err(format!("no capture device with unique ID {id}")))?;
        Ok(Self {
            device,
            unique_id: id,
        })
    }

    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    /// The device's `420v` format at `width`x`height` and its fastest frame duration.
    fn find_format(
        &self,
        width: u32,
        height: u32,
    ) -> Option<(Retained<AVCaptureDeviceFormat>, CMTime, f64)> {
        let mut best: Option<(Retained<AVCaptureDeviceFormat>, CMTime, f64)> = None;
        // SAFETY: plain property reads on a valid device.
        for f in unsafe { self.device.formats() }.iter() {
            let desc = unsafe { f.formatDescription() };
            let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&desc) };
            let subtype = unsafe { desc.media_sub_type() };
            if u32::try_from(dims.width) != Ok(width)
                || u32::try_from(dims.height) != Ok(height)
                || subtype != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            {
                continue;
            }
            for r in unsafe { f.videoSupportedFrameRateRanges() }.iter() {
                let fps = unsafe { r.maxFrameRate() };
                if best.as_ref().is_none_or(|b| fps > b.2) {
                    best = Some((f.clone(), unsafe { r.minFrameDuration() }, fps));
                }
            }
        }
        best
    }

    /// Start capturing `width`x`height` luma at the camera's fastest rate for that size.
    pub fn start_stream(self, width: u32, height: u32) -> io::Result<Stream> {
        let (format, duration, fps) = self.find_format(width, height).ok_or_else(|| {
            err(format!(
                "{} has no 420v {width}x{height} format",
                self.unique_id
            ))
        })?;
        let (w, h) = (width as usize, height as usize);
        let (frames_tx, frames_rx) = mpsc::sync_channel(POOL - 1);
        let (spare_tx, spare_rx) = mpsc::channel();
        for _ in 0..POOL {
            let _ = spare_tx.send(Vec::with_capacity(w * h));
        }
        let sink = FrameSink::new(SinkIvars {
            width: w,
            height: h,
            frames: frames_tx,
            spare: Mutex::new(spare_rx),
            sequence: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
        });

        // SAFETY: AVFoundation calls on valid objects we own, in the documented order: add
        // the input, set the format while locked (so the session preset does not override
        // it), add the output, and stay locked until the session is running.
        unsafe {
            let session = AVCaptureSession::new();
            let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&self.device)
                .map_err(|e| err(format!("camera input: {}", e.localizedDescription())))?;
            session.beginConfiguration();
            if !session.canAddInput(&input) {
                return Err(err("the capture session refused the camera input"));
            }
            session.addInput(&input);
            self.device
                .lockForConfiguration()
                .map_err(|e| err(format!("locking the camera: {}", e.localizedDescription())))?;
            self.device.setActiveFormat(&format);
            self.device.setActiveVideoMinFrameDuration(duration);
            self.device.setActiveVideoMaxFrameDuration(duration);

            let output = AVCaptureVideoDataOutput::new();
            let key: &NSString = &*std::ptr::from_ref(kCVPixelBufferPixelFormatTypeKey).cast();
            let value = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[&value]);
            output.setVideoSettings(Some(&settings));
            output.setAlwaysDiscardsLateVideoFrames(true);
            let queue = DispatchQueue::new("sindenrs.camera", None);
            output.setSampleBufferDelegate_queue(
                Some(ProtocolObject::from_ref(&*sink)),
                Some(&queue),
            );
            if !session.canAddOutput(&output) {
                self.device.unlockForConfiguration();
                return Err(err("the capture session refused the video output"));
            }
            session.addOutput(&output);
            session.commitConfiguration();
            session.startRunning();
            self.device.unlockForConfiguration();
            tracing::info!(
                "capturing {} {width}x{height} 420v at {fps:.1} fps",
                self.unique_id
            );
            Ok(Stream {
                session,
                _output: output,
                sink,
                frames: frames_rx,
                spare: spare_tx,
                current: None,
                width: w,
                height: h,
                fps,
                _device: self,
            })
        }
    }
}

/// A running capture session.
pub struct Stream {
    session: Retained<AVCaptureSession>,
    _output: Retained<AVCaptureVideoDataOutput>,
    sink: Retained<FrameSink>,
    frames: Receiver<Captured>,
    spare: mpsc::Sender<Vec<u8>>,
    /// The frame last handed out, returned to the pool on the next call.
    current: Option<Captured>,
    width: usize,
    height: usize,
    fps: f64,
    _device: Device,
}

impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stream")
            .field("device", &self._device.unique_id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps", &self.fps)
            .finish_non_exhaustive()
    }
}

// SAFETY: the session and output are only touched from here (start/stop), which AVFoundation
// allows from any thread; the sink is shared with its queue by design.
unsafe impl Send for Stream {}

impl Stream {
    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// The frame rate the camera was set to.
    pub fn fps(&self) -> f64 {
        self.fps
    }

    /// Frames dropped so far, by AVFoundation or because the consumer fell behind.
    pub fn dropped(&self) -> u32 {
        self.sink.ivars().dropped.load(Ordering::Relaxed)
    }

    /// The next frame's luma, waiting up to `timeout` (forever if `None`). With `newest`, any
    /// frames already queued behind it are skipped and counted in [`Frame::dropped`].
    pub fn next(
        &mut self,
        timeout: Option<Duration>,
        newest: bool,
    ) -> io::Result<Option<Frame<'_>>> {
        if let Some(done) = self.current.take() {
            let _ = self.spare.send(done.luma);
        }
        let first = match timeout {
            Some(t) => match self.frames.recv_timeout(t) {
                Ok(f) => f,
                Err(RecvTimeoutError::Timeout) => return Ok(None),
                Err(RecvTimeoutError::Disconnected) => return Err(err("capture stopped")),
            },
            None => self.frames.recv().map_err(|_| err("capture stopped"))?,
        };
        let mut frame = first;
        let mut skipped = 0u32;
        if newest {
            while let Ok(f) = self.frames.try_recv() {
                let _ = self.spare.send(std::mem::replace(&mut frame, f).luma);
                skipped += 1;
            }
        }
        let current = self.current.insert(frame);
        Ok(Some(Frame {
            data: &current.luma,
            sequence: current.sequence,
            timestamp: current.timestamp,
            dequeued_at: host_now(),
            dropped: skipped,
            error: false,
        }))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: a running session we own.
        unsafe { self.session.stopRunning() };
    }
}
