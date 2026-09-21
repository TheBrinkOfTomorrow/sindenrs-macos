//! The Sinden serial protocol.
//!
//! Every host-to-gun message is a fixed 7-byte frame: `AA cmd p1 p2 p3 p4 BB`. There is no
//! checksum and no sequence number. Replies are raw, unframed bytes whose length is implied
//! by the command; the gun can also emit unsolicited single-byte events while streaming
//! (see [`event`]). Streaming is gated on a mutual SHA-256 handshake (see [`auth`]).

pub mod auth;
pub mod event;

pub const HEADER: u8 = 0xAA;
pub const TRAILER: u8 = 0xBB;
pub const BAUD: u32 = 115_200;
/// Position axes are 0..=32767, mapping to 0..100% of the screen.
pub const POSITION_MAX: u16 = 32767;

/// Command bytes. Numbers are decimal in the stock driver's source, kept that way here.
pub mod cmd {
    pub const POSITION: u8 = 40;
    pub const POSITION_TRIGGER_LATCH: u8 = 41;
    pub const SECONDARY_SERIAL_ON: u8 = 50;
    pub const SECONDARY_SERIAL_OFF: u8 = 51;
    pub const OFFSCREEN_RELOAD_ON: u8 = 54;
    pub const OFFSCREEN_RELOAD_OFF: u8 = 55;
    pub const BUTTON_MAP: u8 = 60;
    pub const FIRMWARE_VERSION: u8 = 101;
    pub const CAMERA_NAME_READ: u8 = 102;
    pub const CAMERA_NAME_WRITE: u8 = 103;
    pub const CALIBRATION_X_READ: u8 = 104;
    pub const CALIBRATION_Y_READ: u8 = 105;
    pub const CALIBRATION_X_WRITE: u8 = 106;
    pub const CALIBRATION_Y_WRITE: u8 = 107;
    pub const AUTH_HOST_CHALLENGE: u8 = 109;
    pub const UNIQUE_ID: u8 = 111;
    pub const FACTORY_COLOUR: u8 = 113;
    pub const MANUFACTURE_DATE: u8 = 115;
    pub const STREAM_START: u8 = 121;
    pub const RECOIL_ENABLE: u8 = 161;
    pub const RECOIL_AUTO_PARAMS: u8 = 162;
    pub const RECOIL_TRIGGER_MODE: u8 = 163;
    pub const RECOIL_EVENTS: u8 = 164;
    pub const RECOIL_BUTTONS: u8 = 165;
    pub const RECOIL_STRENGTH: u8 = 167;
    pub const RECOIL_TEST_SHOT: u8 = 168;
    pub const RECOIL_TEST_AUTO: u8 = 169;
    pub const AUTH_GUN_CHALLENGE: u8 = 110;
    pub const RECOIL_TIMING: u8 = 171;
    pub const RECOIL_STRENGTH_EXT: u8 = 172;
    pub const CALIBRATION_MODE_ENABLE: u8 = 180;
    pub const RECOIL_TOGGLE_ENABLE: u8 = 181;
    pub const JOYSTICK_MODE: u8 = 182;
    pub const LOW_RESOURCE_MODE: u8 = 183;
    pub const JOYSTICK_PROBE: u8 = 184;
}

/// One host-to-gun frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame(pub [u8; 7]);

impl Frame {
    pub const fn new(cmd: u8, p: [u8; 4]) -> Self {
        Self([HEADER, cmd, p[0], p[1], p[2], p[3], TRAILER])
    }

    /// A command with an all-zero payload.
    pub const fn bare(cmd: u8) -> Self {
        Self::new(cmd, [0; 4])
    }

    /// A command whose only payload is a flag in `p1`.
    pub const fn flag(cmd: u8, on: bool) -> Self {
        Self::new(cmd, [on as u8, 0, 0, 0])
    }

    pub const fn command(&self) -> u8 {
        self.0[1]
    }

    pub const fn as_bytes(&self) -> &[u8; 7] {
        &self.0
    }
}

/// Position report. Axes are clamped to [`POSITION_MAX`] and sent big-endian.
pub fn position(x: u16, y: u16) -> Frame {
    let x = x.min(POSITION_MAX).to_be_bytes();
    let y = y.min(POSITION_MAX).to_be_bytes();
    Frame::new(cmd::POSITION, [x[0], x[1], y[0], y[1]])
}

/// Convert a screen percentage (0..=100) to the 16-bit wire value, as the stock driver does.
pub fn percent_to_axis(pct: f64) -> u16 {
    let pct = pct.clamp(0.0, 100.0);
    // Stock: Convert.ToInt32(pct / 100 * 32767), i.e. round-to-nearest.
    let v = (pct / 100.0 * f64::from(POSITION_MAX)).round();
    // v is within 0..=32767 after the clamp above, so the cast cannot truncate.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = v as u16;
    v
}

pub fn position_percent(x: f64, y: f64) -> Frame {
    position(percent_to_axis(x), percent_to_axis(y))
}

/// Button-map frame: `AA 3C 00 slot 00 value BB`.
pub const fn button_map(slot: u8, value: u8) -> Frame {
    Frame::new(cmd::BUTTON_MAP, [0, slot, 0, value])
}

/// Encode a calibration offset (percent of frame, |v| <= 99) as stored in the gun's EEPROM.
///
/// Wire value is `v * 100 + 10000`, split big-endian into `p1, p2`.
pub fn calibration_encode(percent: f64) -> [u8; 2] {
    let v = (percent.clamp(-99.0, 99.0) * 100.0).round() + 10000.0;
    // 100..=19900 fits in u16.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = v as u16;
    v.to_be_bytes()
}

/// Decode a two-byte calibration reply. Magnitudes over 99% are treated as 0, as stock does.
pub fn calibration_decode(hi: u8, lo: u8) -> f64 {
    let raw = f64::from(u16::from_be_bytes([hi, lo])) - 10000.0;
    let v = raw / 100.0;
    if v.abs() > 99.0 {
        0.0
    } else {
        v
    }
}

pub fn calibration_write_x(percent: f64) -> Frame {
    let [hi, lo] = calibration_encode(percent);
    Frame::new(cmd::CALIBRATION_X_WRITE, [hi, lo, 0, 0])
}

pub fn calibration_write_y(percent: f64) -> Frame {
    let [hi, lo] = calibration_encode(percent);
    Frame::new(cmd::CALIBRATION_Y_WRITE, [hi, lo, 0, 0])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn frame_layout() {
        let f = Frame::new(40, [1, 2, 3, 4]);
        assert_eq!(f.as_bytes(), &[0xAA, 40, 1, 2, 3, 4, 0xBB]);
        assert_eq!(Frame::bare(121).as_bytes(), &[0xAA, 0x79, 0, 0, 0, 0, 0xBB]);
        assert_eq!(
            Frame::flag(180, true).as_bytes(),
            &[0xAA, 0xB4, 1, 0, 0, 0, 0xBB]
        );
    }

    #[test]
    fn position_encoding_matches_stock() {
        // Values from the analysis notes: 50% -> 16383 = 0x3FFF.
        assert_eq!(percent_to_axis(0.0), 0);
        assert_eq!(percent_to_axis(100.0), 32767);
        assert_eq!(percent_to_axis(50.0), 16384); // 16383.5 rounds up
        assert_eq!(percent_to_axis(150.0), 32767);
        assert_eq!(percent_to_axis(-5.0), 0);
        let f = position(0x1234, 0x0ABC);
        assert_eq!(f.as_bytes(), &[0xAA, 0x28, 0x12, 0x34, 0x0A, 0xBC, 0xBB]);
        assert_eq!(
            position(40000, 40000).as_bytes()[2..6],
            [0x7F, 0xFF, 0x7F, 0xFF]
        );
    }

    #[test]
    fn calibration_round_trip() {
        for v in [-99.0, -12.34, 0.0, 0.5, 42.0, 99.0] {
            let [hi, lo] = calibration_encode(v);
            assert!((calibration_decode(hi, lo) - v).abs() < 0.005, "{v}");
        }
        // Zero is exactly 10000 = 0x2710.
        assert_eq!(calibration_encode(0.0), [0x27, 0x10]);
        // Garbage magnitude is treated as zero.
        assert_eq!(calibration_decode(0xFF, 0xFF), 0.0);
        assert_eq!(calibration_decode(0, 0), 0.0);
    }

    #[test]
    fn button_map_layout() {
        assert_eq!(
            button_map(100, 253).as_bytes(),
            &[0xAA, 0x3C, 0, 100, 0, 253, 0xBB]
        );
    }
}
