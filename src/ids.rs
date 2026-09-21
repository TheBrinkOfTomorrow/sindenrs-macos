//! USB identities of Sinden hardware.

/// Vendor ID shared by every gun firmware variant (the V-USB / Teensy shared VID).
pub const GUN_VID: u16 = 0x16c0;

/// Firmware variant, distinguished only by the USB product ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GunVariant {
    Blue,
    Red,
    /// Black, and Shotgun player 1.
    Black,
    /// Player 2 (any colour), and Shotgun player 2.
    Player2,
}

impl GunVariant {
    pub fn from_pid(pid: u16) -> Option<Self> {
        match pid {
            0x0f01 => Some(Self::Blue),
            0x0f02 => Some(Self::Red),
            0x0f38 => Some(Self::Black),
            0x0f39 => Some(Self::Player2),
            _ => None,
        }
    }

    pub fn pid(self) -> u16 {
        match self {
            Self::Blue => 0x0f01,
            Self::Red => 0x0f02,
            Self::Black => 0x0f38,
            Self::Player2 => 0x0f39,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Blue => "Blue",
            Self::Red => "Red",
            Self::Black => "Black / Shotgun P1",
            Self::Player2 => "Player 2 / Shotgun P2",
        }
    }
}

/// All gun product IDs, for udev rules and scanning.
pub const GUN_PIDS: [u16; 4] = [0x0f01, 0x0f02, 0x0f38, 0x0f39];

/// (vendor, product) pairs of the camera boards shipped in guns.
pub const CAMERA_IDS: [(u16, u16); 3] = [(0x05a3, 0x9210), (0x32e4, 0x9210), (0x16d0, 0x0109)];

pub fn is_gun(vid: u16, pid: u16) -> bool {
    vid == GUN_VID && GUN_PIDS.contains(&pid)
}

pub fn is_camera(vid: u16, pid: u16) -> bool {
    CAMERA_IDS.contains(&(vid, pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_round_trip() {
        for pid in GUN_PIDS {
            let v = GunVariant::from_pid(pid).expect("known pid");
            assert_eq!(v.pid(), pid);
            assert!(is_gun(GUN_VID, pid));
        }
        assert!(GunVariant::from_pid(0x0f00).is_none());
        assert!(!is_gun(0x16c1, 0x0f01));
    }
}
