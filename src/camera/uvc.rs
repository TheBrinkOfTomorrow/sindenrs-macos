//! UVC camera controls as raw class requests, for platforms whose camera API does not expose
//! them (macOS: AVFoundation offers no manual exposure on external cameras).
//!
//! This part is pure: finding the VideoControl interface and its entities in the configuration
//! descriptor, and mapping the V4L2 control IDs the config speaks ([`super::cid`]) to UVC
//! selectors. uvcvideo passes these controls through unscaled, so a value means the same here
//! as on Linux. The transport lives with the platform (`iokit` on macOS).

use super::cid;

#[cfg(target_os = "macos")]
pub mod iokit;

/// UVC 1.1 request codes.
pub mod req {
    pub const SET_CUR: u8 = 0x01;
    pub const GET_CUR: u8 = 0x81;
    pub const GET_MIN: u8 = 0x82;
    pub const GET_MAX: u8 = 0x83;
    pub const GET_RES: u8 = 0x84;
    pub const GET_DEF: u8 = 0x87;
}

/// Where a control lives: the camera terminal or the processing unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entity {
    CameraTerminal,
    ProcessingUnit,
}

/// One control as UVC addresses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selector {
    pub entity: Entity,
    pub selector: u8,
    /// Payload length in bytes.
    pub len: u8,
    pub signed: bool,
    /// Bit in the entity's `bmControls` that says the camera supports it.
    pub bit: u8,
}

/// The UVC selector for a V4L2 control ID, if it is one we map.
pub const fn selector(id: u32) -> Option<Selector> {
    use Entity::{CameraTerminal as Ct, ProcessingUnit as Pu};
    let (entity, selector, len, signed, bit) = match id {
        cid::EXPOSURE_AUTO => (Ct, 0x02, 1, false, 1),
        cid::EXPOSURE_ABSOLUTE => (Ct, 0x04, 4, false, 3),
        cid::ZOOM_ABSOLUTE => (Ct, 0x0b, 2, false, 9),
        cid::BACKLIGHT_COMPENSATION => (Pu, 0x01, 2, false, 8),
        cid::BRIGHTNESS => (Pu, 0x02, 2, true, 0),
        cid::CONTRAST => (Pu, 0x03, 2, false, 1),
        cid::GAIN => (Pu, 0x04, 2, false, 9),
        cid::POWER_LINE_FREQUENCY => (Pu, 0x05, 1, false, 10),
        cid::HUE => (Pu, 0x06, 2, true, 2),
        cid::SATURATION => (Pu, 0x07, 2, false, 3),
        cid::SHARPNESS => (Pu, 0x08, 2, false, 4),
        cid::GAMMA => (Pu, 0x09, 2, false, 5),
        cid::WHITE_BALANCE_TEMPERATURE => (Pu, 0x0a, 2, false, 6),
        cid::AUTO_WHITE_BALANCE => (Pu, 0x0b, 1, false, 12),
        _ => return None,
    };
    Some(Selector {
        entity,
        selector,
        len,
        signed,
        bit,
    })
}

/// Every control [`selector`] maps, with its name, for listings.
pub const CONTROLS: [(u32, &str); 14] = [
    (cid::EXPOSURE_AUTO, "exposure_auto"),
    (cid::EXPOSURE_ABSOLUTE, "exposure_absolute"),
    (cid::ZOOM_ABSOLUTE, "zoom_absolute"),
    (cid::BRIGHTNESS, "brightness"),
    (cid::CONTRAST, "contrast"),
    (cid::SATURATION, "saturation"),
    (cid::HUE, "hue"),
    (cid::GAMMA, "gamma"),
    (cid::GAIN, "gain"),
    (cid::SHARPNESS, "sharpness"),
    (cid::BACKLIGHT_COMPENSATION, "backlight_compensation"),
    (cid::POWER_LINE_FREQUENCY, "power_line_frequency"),
    (cid::WHITE_BALANCE_TEMPERATURE, "white_balance_temperature"),
    (cid::AUTO_WHITE_BALANCE, "white_balance_automatic"),
];

/// `V4L2_CID_EXPOSURE_AUTO` menu value to the UVC AE mode bitmap, and back.
pub const fn ae_mode_from_v4l2(v: i32) -> Option<u8> {
    match v {
        0 => Some(0x02), // auto
        1 => Some(0x01), // manual
        2 => Some(0x04), // shutter priority
        3 => Some(0x08), // aperture priority
        _ => None,
    }
}

pub const fn ae_mode_to_v4l2(mode: u8) -> i32 {
    match mode {
        0x01 => 1,
        0x02 => 0,
        0x04 => 2,
        _ => 3,
    }
}

/// Decode a little-endian control payload.
pub fn decode(bytes: &[u8], signed: bool) -> i32 {
    match (bytes.len(), signed) {
        (1, _) => i32::from(bytes[0]),
        (2, false) => i32::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        (2, true) => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        (4, _) => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        _ => 0,
    }
}

/// Encode a control value as its little-endian payload of `len` bytes (truncating, as the
/// camera only reads `len` bytes).
pub fn encode(v: i32, len: u8) -> Vec<u8> {
    v.to_le_bytes()[..usize::from(len)].to_vec()
}

/// The VideoControl interface and the entities controls are addressed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Topology {
    pub interface: u8,
    pub camera_terminal: u8,
    pub processing_unit: u8,
    /// `bmControls` of the camera terminal and the processing unit, as little-endian bitmaps.
    pub ct_controls: u32,
    pub pu_controls: u32,
}

impl Topology {
    /// Find the VideoControl interface (class 14, subclass 1) and, among its class-specific
    /// descriptors, the camera input terminal (type 0x0201) and the processing unit.
    pub fn parse(config: &[u8]) -> Option<Self> {
        let mut vc_if = None;
        let (mut ct, mut pu) = (None, None);
        let mut in_vc = false;
        let mut i = 0;
        while i + 2 <= config.len() {
            let d = &config[i..];
            let len = usize::from(d[0]);
            if len < 2 || len > d.len() {
                break;
            }
            let d = &d[..len];
            match d[1] {
                // INTERFACE
                0x04 if len >= 9 => {
                    in_vc = d[5] == 14 && d[6] == 1;
                    if in_vc && vc_if.is_none() {
                        vc_if = Some(d[2]);
                    }
                }
                // CS_INTERFACE within VideoControl
                0x24 if in_vc && len >= 3 => match d[2] {
                    // INPUT_TERMINAL of type camera: bControlSize at 14, bmControls from 15.
                    0x02 if len >= 15 && u16::from_le_bytes([d[4], d[5]]) == 0x0201 => {
                        ct = Some((d[3], bitmap(&d[15..], usize::from(d[14]))));
                    }
                    // PROCESSING_UNIT: bControlSize at 7, bmControls from 8.
                    0x05 if len >= 8 => {
                        pu = Some((d[3], bitmap(&d[8..], usize::from(d[7]))));
                    }
                    _ => {}
                },
                _ => {}
            }
            i += len;
        }
        let (camera_terminal, ct_controls) = ct?;
        let (processing_unit, pu_controls) = pu?;
        Some(Self {
            interface: vc_if?,
            camera_terminal,
            processing_unit,
            ct_controls,
            pu_controls,
        })
    }

    pub const fn entity_id(&self, e: Entity) -> u8 {
        match e {
            Entity::CameraTerminal => self.camera_terminal,
            Entity::ProcessingUnit => self.processing_unit,
        }
    }

    /// Whether the camera advertises the control.
    pub const fn supports(&self, s: Selector) -> bool {
        let map = match s.entity {
            Entity::CameraTerminal => self.ct_controls,
            Entity::ProcessingUnit => self.pu_controls,
        };
        map & (1 << s.bit) != 0
    }

    /// `wValue` and `wIndex` of a request for `s`.
    pub const fn address(&self, s: Selector) -> (u16, u16) {
        (
            (s.selector as u16) << 8,
            ((self.entity_id(s.entity) as u16) << 8) | self.interface as u16,
        )
    }
}

fn bitmap(bytes: &[u8], size: usize) -> u32 {
    bytes
        .iter()
        .take(size.min(4))
        .enumerate()
        .fold(0, |acc, (i, &b)| acc | u32::from(b) << (8 * i))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Sinden camera's VideoControl part, as `uvcctl` read it on hardware: interface 0,
    /// camera terminal 1 (bmControls 0a 22 00), processing unit 3 (bmControls 7f 15), with a
    /// VideoStreaming interface after it whose descriptors must be ignored.
    fn sinden_config() -> Vec<u8> {
        let mut c = vec![9, 0x02, 0, 0, 2, 1, 0, 0x80, 250]; // CONFIGURATION (length unused)
        c.extend([9, 0x04, 0, 0, 1, 14, 1, 0, 0]); // INTERFACE 0: VideoControl
        c.extend([13, 0x24, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0, 1, 1]); // VC header
        c.extend([
            18, 0x24, 0x02, 1, 0x01, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0x0a, 0x22, 0x00,
        ]);
        c.extend([11, 0x24, 0x05, 3, 1, 0, 0, 2, 0x7f, 0x15, 0]); // PROCESSING_UNIT
        c.extend([9, 0x04, 1, 0, 0, 14, 2, 0, 0]); // INTERFACE 1: VideoStreaming
        c.extend([5, 0x24, 0x05, 9, 9]); // a VS descriptor that looks like a PU subtype
        c
    }

    #[test]
    fn parses_sinden_topology() {
        let t = Topology::parse(&sinden_config()).expect("topology");
        assert_eq!(t.interface, 0);
        assert_eq!(t.camera_terminal, 1);
        assert_eq!(t.processing_unit, 3);
        assert_eq!(t.ct_controls, 0x22_0a);
        assert_eq!(t.pu_controls, 0x15_7f);
    }

    #[test]
    fn supported_controls_match_hardware() {
        let t = Topology::parse(&sinden_config()).expect("topology");
        let has = |id| selector(id).is_some_and(|s| t.supports(s));
        assert!(has(cid::EXPOSURE_AUTO));
        assert!(has(cid::EXPOSURE_ABSOLUTE));
        assert!(has(cid::BRIGHTNESS));
        assert!(has(cid::CONTRAST));
        assert!(has(cid::GAMMA));
        assert!(has(cid::POWER_LINE_FREQUENCY));
        // GET on gain failed on hardware, and the bitmap agrees.
        assert!(!has(cid::GAIN));
    }

    #[test]
    fn request_addresses() {
        let t = Topology::parse(&sinden_config()).expect("topology");
        let exposure = selector(cid::EXPOSURE_ABSOLUTE).expect("mapped");
        assert_eq!(t.address(exposure), (0x0400, 0x0100));
        let contrast = selector(cid::CONTRAST).expect("mapped");
        assert_eq!(t.address(contrast), (0x0300, 0x0300));
    }

    #[test]
    fn payloads_round_trip() {
        assert_eq!(encode(78, 4), vec![78, 0, 0, 0]);
        assert_eq!(decode(&encode(78, 4), false), 78);
        assert_eq!(decode(&encode(-5, 2), true), -5);
        assert_eq!(decode(&[0xfb, 0xff], false), 0xfffb);
        assert_eq!(encode(8, 1), vec![8]);
    }

    #[test]
    fn exposure_modes() {
        assert_eq!(ae_mode_from_v4l2(cid::EXPOSURE_MANUAL), Some(1));
        assert_eq!(ae_mode_from_v4l2(cid::EXPOSURE_APERTURE_PRIORITY), Some(8));
        assert_eq!(ae_mode_to_v4l2(8), cid::EXPOSURE_APERTURE_PRIORITY);
        assert_eq!(ae_mode_to_v4l2(1), cid::EXPOSURE_MANUAL);
    }

    #[test]
    fn truncated_descriptors_are_rejected() {
        let c = sinden_config();
        assert!(Topology::parse(&c[..20]).is_none());
        assert!(Topology::parse(&[]).is_none());
    }
}
