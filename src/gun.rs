//! A serial session with one gun.
//!
//! All reads are bounded by deadlines rather than the stock driver's fixed sleeps, and every
//! query reports how long the gun took to answer so the startup budget can be measured
//! rather than assumed.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};
use thiserror::Error;
use tracing::{debug, trace};

use crate::protocol::event::{Event, EventParser};
use crate::protocol::{auth, calibration_decode, cmd, Frame};

#[derive(Debug, Error)]
pub enum GunError {
    #[error("serial port error: {0}")]
    Serial(#[from] serialport::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for reply to command {command}: wanted {wanted} bytes, got {got}")]
    Timeout {
        command: u8,
        wanted: usize,
        got: usize,
    },
    #[error("gun is not reading its serial port ({pending} bytes unsent after {waited:?}); its firmware has wedged and needs a power cycle")]
    NotReading { pending: u32, waited: Duration },
    #[error("configuration error: {0}")]
    Config(String),
    #[error("gun failed authentication: its response to our nonce did not match")]
    GunAuthFailed,
    #[error("gun rejected our authentication: replied {0:?} instead of \"true\"")]
    HostAuthRejected(String),
}

pub type Result<T> = std::result::Result<T, GunError>;

/// How long the gun took to answer a query.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    /// Time from the end of our write to the first reply byte.
    pub first_byte: Duration,
    /// Time from the end of our write to the last expected byte.
    pub complete: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AuthReport {
    pub leg1: Timing,
    pub leg2_challenge: Timing,
    pub leg2_verdict: Timing,
}

/// Default deadline for a reply to a single query. The stock driver sleeps 50-200 ms and hopes.
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// A healthy gun drains a 7-byte frame in well under a millisecond. If our bytes are still
/// queued after this long, the firmware has stopped reading.
pub const NOT_READING_AFTER: Duration = Duration::from_millis(300);

pub struct Gun {
    port: Box<dyn SerialPort>,
    path: String,
    events: EventParser,
    last_auth: Option<AuthReport>,
}

impl Gun {
    /// Open the gun's CDC-ACM port: 115200 8N1, no flow control, DTR and RTS asserted.
    pub fn open(path: &str) -> Result<Self> {
        let mut port = serialport::new(path, crate::protocol::BAUD)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(Duration::from_millis(20))
            .open()?;
        port.write_data_terminal_ready(true)?;
        port.write_request_to_send(true)?;
        debug!(path, "opened gun serial port");
        Ok(Self {
            port,
            path: path.to_owned(),
            events: EventParser::new(),
            last_auth: None,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Timings from the most recent successful [`Gun::authenticate`].
    pub fn last_auth(&self) -> Option<AuthReport> {
        self.last_auth
    }

    /// Write one frame and flush it to the device.
    pub fn send(&mut self, frame: Frame) -> Result<()> {
        trace!(bytes = ?frame.as_bytes(), "tx");
        // No flush(): on a tty that is tcdrain(), which blocks with no timeout if the gun has
        // stopped reading. The kernel submits USB writes immediately anyway.
        self.port.write_all(frame.as_bytes())?;
        Ok(())
    }

    fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        trace!(?bytes, "tx raw");
        self.port.write_all(bytes)?;
        Ok(())
    }

    /// Discard anything the gun has already sent.
    pub fn discard_input(&mut self) -> Result<()> {
        self.port.clear(ClearBuffer::Input)?;
        Ok(())
    }

    /// Read exactly `buf.len()` bytes before `deadline`, returning reply timing.
    fn read_exact_by(
        &mut self,
        command: u8,
        buf: &mut [u8],
        start: Instant,
        deadline: Instant,
    ) -> Result<Timing> {
        let mut got = 0;
        let mut first: Option<Duration> = None;
        while got < buf.len() {
            let now = Instant::now();
            if now >= deadline {
                return Err(GunError::Timeout {
                    command,
                    wanted: buf.len(),
                    got,
                });
            }
            if got == 0 && now - start >= NOT_READING_AFTER {
                let pending = self.port.bytes_to_write().unwrap_or(0);
                if pending > 0 {
                    return Err(GunError::NotReading {
                        pending,
                        waited: now - start,
                    });
                }
            }
            match self.port.read(&mut buf[got..]) {
                Ok(0) => {}
                Ok(n) => {
                    if first.is_none() {
                        first = Some(start.elapsed());
                    }
                    got += n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let complete = start.elapsed();
        trace!(command, bytes = ?buf, ?complete, "rx");
        Ok(Timing {
            first_byte: first.unwrap_or(complete),
            complete,
        })
    }

    /// Send a command and read a fixed-length reply.
    pub fn query(&mut self, cmd: u8, reply: &mut [u8], timeout: Duration) -> Result<Timing> {
        self.discard_input()?;
        self.send(Frame::bare(cmd))?;
        let start = Instant::now();
        self.read_exact_by(cmd, reply, start, start + timeout)
    }

    /// Send a command and read a variable-length reply: everything that arrives until the
    /// line has been quiet for `quiet`, or `max` bytes, or `timeout` overall.
    pub fn query_variable(
        &mut self,
        cmd: u8,
        max: usize,
        quiet: Duration,
        timeout: Duration,
    ) -> Result<(Vec<u8>, Timing)> {
        self.discard_input()?;
        self.send(Frame::bare(cmd))?;
        let start = Instant::now();
        let deadline = start + timeout;
        let mut out = Vec::new();
        let mut first: Option<Duration> = None;
        let mut last_rx = Instant::now();
        let mut chunk = [0u8; 64];
        loop {
            let now = Instant::now();
            if now >= deadline || out.len() >= max || (first.is_some() && now - last_rx >= quiet) {
                break;
            }
            match self.port.read(&mut chunk) {
                Ok(n) if n > 0 => {
                    if first.is_none() {
                        first = Some(start.elapsed());
                    }
                    out.extend_from_slice(&chunk[..n]);
                    last_rx = Instant::now();
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let complete = first.map_or(Duration::ZERO, |_| last_rx - start);
        Ok((
            out,
            Timing {
                first_byte: first.unwrap_or(complete),
                complete,
            },
        ))
    }

    /// Complete a pending auth read in the firmware: if a 110/109 command reached the gun
    /// without its 32-byte payload, the firmware sits waiting for exactly that. Feeding it
    /// 32 bytes in one write lets it finish (it answers with a hash we ignore). Cheap first
    /// step before a bootloader reset; only works while the gun still accepts USB packets.
    pub fn unwedge(&mut self) -> Result<()> {
        self.send_raw(&[0u8; 32])?;
        std::thread::sleep(Duration::from_millis(500));
        self.discard_input()?;
        Ok(())
    }

    /// Perform both legs of the handshake. Streaming is refused until this succeeds.
    pub fn authenticate(&mut self) -> Result<AuthReport> {
        let mut report = AuthReport::default();
        let timeout = Duration::from_secs(2);

        // Leg 1: we challenge the gun.
        self.discard_input()?;
        let nonce = auth::make_nonce();
        // One write for command + payload: a 110 that reaches the gun without its 32 bytes
        // leaves the firmware blocked in a fixed-length read until it is physically reset.
        let mut msg = [0u8; 7 + 32];
        msg[..7].copy_from_slice(Frame::bare(cmd::AUTH_GUN_CHALLENGE).as_bytes());
        msg[7..].copy_from_slice(&nonce);
        self.send_raw(&msg)?;
        let start = Instant::now();
        let mut response = [0u8; 32];
        report.leg1 = self.read_exact_by(
            cmd::AUTH_GUN_CHALLENGE,
            &mut response,
            start,
            start + timeout,
        )?;
        if response != auth::expected_gun_response(&nonce) {
            return Err(GunError::GunAuthFailed);
        }
        debug!(?report.leg1, "gun authenticated to us");

        // Leg 2: the gun challenges us.
        self.discard_input()?;
        self.send(Frame::bare(cmd::AUTH_HOST_CHALLENGE))?;
        let start = Instant::now();
        let mut challenge = [0u8; 32];
        report.leg2_challenge = self.read_exact_by(
            cmd::AUTH_HOST_CHALLENGE,
            &mut challenge,
            start,
            start + timeout,
        )?;
        self.send_raw(&auth::host_response(&challenge))?;
        let start = Instant::now();
        let mut verdict = [0u8; 5];
        report.leg2_verdict = self.read_exact_by(
            cmd::AUTH_HOST_CHALLENGE,
            &mut verdict,
            start,
            start + timeout,
        )?;
        let text = String::from_utf8_lossy(&verdict).trim_end().to_owned();
        if text != "true" {
            return Err(GunError::HostAuthRejected(text));
        }
        debug!(?report.leg2_verdict, "we authenticated to gun");
        self.last_auth = Some(report);
        Ok(report)
    }

    /// Begin streaming: command 121, sent twice with a gap (stock uses 150 ms).
    pub fn start_streaming(&mut self, gap: Duration) -> Result<()> {
        self.send(Frame::bare(cmd::STREAM_START))?;
        std::thread::sleep(gap);
        self.send(Frame::bare(cmd::STREAM_START))
    }

    pub fn firmware_version(&mut self) -> Result<((u8, u8), Timing)> {
        let mut b = [0u8; 2];
        let t = self.query(cmd::FIRMWARE_VERSION, &mut b, QUERY_TIMEOUT)?;
        Ok(((b[0], b[1]), t))
    }

    /// The camera name stored in the gun (15 ASCII bytes, space padded).
    pub fn camera_name(&mut self) -> Result<(String, Timing)> {
        let mut b = [0u8; 15];
        let t = self.query(cmd::CAMERA_NAME_READ, &mut b, QUERY_TIMEOUT)?;
        Ok((String::from_utf8_lossy(&b).trim_end().to_owned(), t))
    }

    fn calibration(&mut self, command: u8) -> Result<(f64, Timing)> {
        let mut b = [0u8; 2];
        let t = self.query(command, &mut b, QUERY_TIMEOUT)?;
        Ok((calibration_decode(b[0], b[1]), t))
    }

    pub fn calibration_x(&mut self) -> Result<(f64, Timing)> {
        self.calibration(cmd::CALIBRATION_X_READ)
    }

    pub fn calibration_y(&mut self) -> Result<(f64, Timing)> {
        self.calibration(cmd::CALIBRATION_Y_READ)
    }

    /// The gun's unique id as a decimal string (command 111, one digit per byte).
    pub fn unique_id(&mut self) -> Result<String> {
        let (bytes, _) = self.identity(cmd::UNIQUE_ID)?;
        Ok(bytes.iter().map(ToString::to_string).collect())
    }

    /// Variable-length identity replies (111 unique ID, 113 factory colour, 115 date).
    pub fn identity(&mut self, command: u8) -> Result<(Vec<u8>, Timing)> {
        self.query_variable(command, 64, Duration::from_millis(60), QUERY_TIMEOUT)
    }

    /// Persistently enable or disable the joystick HID device (command 184, p1 = 1/0).
    /// Takes effect at the next USB enumeration, so follow it with a reset.
    pub fn set_joystick_device(&mut self, enabled: bool) -> Result<()> {
        self.send(Frame::new(
            cmd::JOYSTICK_PROBE,
            [u8::from(enabled), 0, 0, 0],
        ))?;
        std::thread::sleep(Duration::from_millis(100));
        self.discard_input()
    }

    /// Joystick hardware probe: command 184 with p1 = 2; any non-zero byte means present.
    pub fn joystick_probe(&mut self) -> Result<(bool, Timing)> {
        self.discard_input()?;
        self.send(Frame::new(cmd::JOYSTICK_PROBE, [2, 0, 0, 0]))?;
        let start = Instant::now();
        let mut b = [0u8; 1];
        let t = self.read_exact_by(cmd::JOYSTICK_PROBE, &mut b, start, start + QUERY_TIMEOUT)?;
        Ok((b[0] != 0, t))
    }

    pub fn set_position(&mut self, x: u16, y: u16) -> Result<()> {
        self.send(crate::protocol::position(x, y))
    }

    pub fn set_offscreen_reload(&mut self, on: bool) -> Result<()> {
        self.send(Frame::bare(if on {
            cmd::OFFSCREEN_RELOAD_ON
        } else {
            cmd::OFFSCREEN_RELOAD_OFF
        }))
    }

    pub fn set_calibration_mode_enabled(&mut self, on: bool) -> Result<()> {
        self.send(Frame::flag(cmd::CALIBRATION_MODE_ENABLE, on))
    }

    pub fn set_recoil_enabled(&mut self, on: bool) -> Result<()> {
        self.send(Frame::flag(cmd::RECOIL_ENABLE, on))
    }

    pub fn set_recoil_toggle_enabled(&mut self, on: bool) -> Result<()> {
        self.send(Frame::flag(cmd::RECOIL_TOGGLE_ENABLE, on))
    }

    pub fn set_joystick_mode(&mut self, on: bool) -> Result<()> {
        self.send(Frame::flag(cmd::JOYSTICK_MODE, on))
    }

    pub fn set_low_resource_mode(&mut self, on: bool) -> Result<()> {
        self.send(Frame::flag(cmd::LOW_RESOURCE_MODE, on))
    }

    pub fn set_secondary_serial(&mut self, on: bool) -> Result<()> {
        self.send(Frame::bare(if on {
            cmd::SECONDARY_SERIAL_ON
        } else {
            cmd::SECONDARY_SERIAL_OFF
        }))
    }

    /// Send everything the vendor driver sends at startup, from config: modes, button map,
    /// recoil. `recoil_gap` is the pause between recoil-configuration frames (vendor: 100 ms).
    pub fn apply_config(
        &mut self,
        cfg: &crate::config::GunConfig,
        recoil_gap: Duration,
    ) -> Result<()> {
        self.set_offscreen_reload(cfg.offscreen_reload)?;
        let frames = cfg
            .buttons
            .frames()
            .map_err(|e| GunError::Config(e.to_string()))?;
        for f in &frames {
            self.send(*f)?;
        }
        debug!(n = frames.len(), "button map sent");
        self.send_recoil_burst(&cfg.recoil, recoil_gap)?;
        self.set_calibration_mode_enabled(cfg.calibration_mode)?;
        self.set_secondary_serial(false)?;
        self.set_recoil_toggle_enabled(cfg.recoil_toggle)?;
        self.set_joystick_mode(cfg.joystick)?;
        self.set_low_resource_mode(false)?;
        Ok(())
    }

    /// Send the recoil configuration frames. Three of them (167, 171, 172) answer with a few
    /// bytes about 2 ms later; each frame is followed by `gap` and a drain so those bytes never
    /// end up in front of a later query's reply. 5 ms is enough; the vendor uses 100.
    pub fn send_recoil_burst(
        &mut self,
        recoil: &crate::config::Recoil,
        gap: Duration,
    ) -> Result<()> {
        for f in recoil.frames() {
            self.send(f)?;
            std::thread::sleep(gap);
            self.discard_input()?;
        }
        Ok(())
    }

    /// Fire one recoil pulse now (command 168). Needs recoil enabled (161).
    pub fn fire_recoil(&mut self) -> Result<()> {
        self.send(Frame::bare(cmd::RECOIL_TEST_SHOT))
    }

    /// Start automatic recoil (command 169); stop it with [`Gun::set_recoil_enabled`].
    pub fn start_auto_recoil(&mut self) -> Result<()> {
        self.send(Frame::bare(cmd::RECOIL_TEST_AUTO))
    }

    /// Persist calibration offsets to the gun's EEPROM.
    pub fn write_calibration(&mut self, x_percent: f64, y_percent: f64) -> Result<()> {
        self.send(crate::protocol::calibration_write_x(x_percent))?;
        std::thread::sleep(Duration::from_millis(50));
        self.send(crate::protocol::calibration_write_y(y_percent))
    }

    /// Throw away unsent output. Needed before closing a port whose gun has stopped reading:
    /// cdc-acm otherwise waits `closing_wait` (30 s by default) for the stuck URBs on close.
    pub fn discard_output(&mut self) -> Result<()> {
        self.port.clear(ClearBuffer::Output)?;
        Ok(())
    }

    /// Read whatever bytes are pending, undecoded. Never blocks.
    pub fn read_available(&mut self) -> Result<Vec<u8>> {
        let avail = self.port.bytes_to_read()? as usize;
        if avail == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; avail.min(512)];
        let n = match self.port.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(e) => return Err(e.into()),
        };
        buf.truncate(n);
        Ok(buf)
    }

    /// Drain whatever the gun has sent and decode it as events. Never blocks.
    pub fn poll_events(&mut self) -> Result<Vec<Event>> {
        let avail = self.port.bytes_to_read()? as usize;
        if avail == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; avail.min(256)];
        let n = match self.port.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(self.events.feed(&buf[..n]))
    }
}

/// Reset the gun's microcontroller through its bootloader: open the CDC port at 1200 baud
/// with DTR low, exactly as avrdude does for a Leonardo. The Arduino USB core handles this
/// in the USB interrupt, so it works even when the sketch's main loop is stuck. The gun
/// disconnects, shows up as the Caterina bootloader (`2341:0036`) for a few seconds, then
/// re-enumerates as itself.
pub fn bootloader_touch(path: &str) -> Result<()> {
    let mut port = serialport::new(path, 1200)
        .dtr_on_open(false)
        .timeout(Duration::from_millis(100))
        .open()?;
    port.write_data_terminal_ready(false)?;
    std::thread::sleep(Duration::from_millis(100));
    drop(port);
    debug!(path, "sent 1200-baud reset touch");
    Ok(())
}

impl Drop for Gun {
    fn drop(&mut self) {
        // A healthy gun has consumed everything within a millisecond; a wedged one never will,
        // and without this the close blocks for the kernel's closing_wait.
        let _ = self.port.clear(ClearBuffer::Output);
    }
}
