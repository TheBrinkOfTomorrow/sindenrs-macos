//! Firmware images and the Caterina (AVR109) bootloader.
//!
//! The gun is an ATmega32U4 with the Arduino Leonardo bootloader, which is what the vendor's
//! Windows app talks to (via the .NET ArduinoSketchUploader). Entering it is the 1200-baud
//! touch already used for resets; it then enumerates as `2341:0036` with its own CDC port and
//! speaks the AVR109 "butterfly" protocol. Only the application section (below 0x7000) is
//! ever written; the bootloader section in the image is compared but never touched.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use thiserror::Error;
use tracing::{debug, info};

/// Arduino Leonardo / Caterina bootloader USB identity.
pub const BOOTLOADER_VID: u16 = 0x2341;
pub const BOOTLOADER_PID: u16 = 0x0036;
/// ATmega32U4 flash size and Caterina boot section start (4 KiB bootloader).
pub const FLASH_SIZE: u32 = 0x8000;
pub const BOOT_START: u32 = 0x7000;
pub const PAGE_SIZE: u32 = 128;

#[derive(Debug, Error)]
pub enum FirmwareError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serial error: {0}")]
    Serial(#[from] serialport::Error),
    #[error("bad Intel HEX at line {line}: {reason}")]
    Hex { line: usize, reason: String },
    #[error("bootloader replied {got:?} to '{cmd}' (expected {expected:?})")]
    Protocol {
        cmd: char,
        expected: String,
        got: Vec<u8>,
    },
    #[error("bootloader timed out on '{0}'")]
    Timeout(char),
    #[error("verify failed at {addr:#06x}: wrote {expected:#04x}, read {got:#04x}")]
    Verify { addr: u32, expected: u8, got: u8 },
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, FirmwareError>;

/// A sparse flash image.
#[derive(Clone, Debug, Default)]
pub struct Image {
    pub bytes: BTreeMap<u32, u8>,
}

impl Image {
    pub fn parse_intel_hex(text: &str) -> Result<Self> {
        let mut bytes = BTreeMap::new();
        let mut base: u32 = 0;
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let err = |reason: &str| FirmwareError::Hex {
                line: i + 1,
                reason: reason.to_owned(),
            };
            let Some(hex) = line.strip_prefix(':') else {
                return Err(err("missing ':'"));
            };
            if hex.len() < 10 || hex.len() % 2 != 0 {
                return Err(err("bad length"));
            }
            let data: Vec<u8> = (0..hex.len() / 2)
                .map(|k| {
                    u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).map_err(|_| err("bad hex digit"))
                })
                .collect::<Result<_>>()?;
            let n = data[0] as usize;
            if data.len() != n + 5 {
                return Err(err("byte count mismatch"));
            }
            if data.iter().fold(0u8, |a, &b| a.wrapping_add(b)) != 0 {
                return Err(err("checksum"));
            }
            let addr = u32::from(u16::from_be_bytes([data[1], data[2]]));
            match data[3] {
                0 => {
                    for (k, &b) in data[4..4 + n].iter().enumerate() {
                        // n <= 255, so k always fits.
                        bytes.insert(base + addr + u32::try_from(k).unwrap_or(0), b);
                    }
                }
                1 => break,
                2 => base = u32::from(u16::from_be_bytes([data[4], data[5]])) << 4,
                4 => base = u32::from(u16::from_be_bytes([data[4], data[5]])) << 16,
                3 | 5 => {}
                t => return Err(err(&format!("unsupported record type {t}"))),
            }
        }
        Ok(Self { bytes })
    }

    pub fn from_binary(data: &[u8]) -> Self {
        Self {
            bytes: data
                .iter()
                .enumerate()
                .map(|(i, &b)| (u32::try_from(i).unwrap_or(u32::MAX), b))
                .collect(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path)?;
        if data.first() == Some(&b':') {
            Self::parse_intel_hex(&String::from_utf8_lossy(&data))
        } else {
            Ok(Self::from_binary(&data))
        }
    }

    pub fn max_addr(&self) -> Option<u32> {
        self.bytes.keys().next_back().copied()
    }

    /// Highest address below the boot section, if any application data exists.
    pub fn app_max_addr(&self) -> Option<u32> {
        self.bytes.range(..BOOT_START).next_back().map(|(&a, _)| a)
    }

    pub fn has_boot_section(&self) -> bool {
        self.bytes.range(BOOT_START..).next().is_some()
    }

    /// Flat copy of `[start, end)` with 0xFF where the image has no data.
    pub fn flat(&self, start: u32, end: u32) -> Vec<u8> {
        (start..end)
            .map(|a| self.bytes.get(&a).copied().unwrap_or(0xFF))
            .collect()
    }

    /// The USB product ID embedded in the device descriptor, if found (identifies the variant).
    pub fn usb_ids(&self) -> Option<(u16, u16)> {
        let flat = self.flat(0, BOOT_START);
        // 12 01 bcdUSB(2) class sub proto 40 vid(2) pid(2)
        flat.windows(12).find_map(|w| {
            (w[0] == 0x12 && w[1] == 0x01 && w[7] == 0x40).then(|| {
                (
                    u16::from_le_bytes([w[8], w[9]]),
                    u16::from_le_bytes([w[10], w[11]]),
                )
            })
        })
    }

    pub fn to_intel_hex(&self) -> String {
        let mut out = String::new();
        let mut addr = 0u32;
        let end = self.max_addr().map_or(0, |a| a + 1);
        while addr < end {
            let chunk: Vec<u8> = (0..16)
                .filter_map(|k| self.bytes.get(&(addr + k)).copied())
                .collect();
            if chunk.len() == 16 {
                // Records carry the low 16 bits; we never exceed 64 KiB.
                let [_, _, hi, lo] = addr.to_be_bytes();
                let mut rec = vec![16u8, hi, lo, 0];
                rec.extend(&chunk);
                let sum = rec
                    .iter()
                    .fold(0u8, |a, &b| a.wrapping_add(b))
                    .wrapping_neg();
                out.push(':');
                for b in rec.iter().chain(std::iter::once(&sum)) {
                    out.push_str(&format!("{b:02X}"));
                }
                out.push('\n');
            }
            addr += 16;
        }
        out.push_str(":00000001FF\n");
        out
    }
}

/// A session with the Caterina bootloader over its CDC port.
pub struct Bootloader {
    port: Box<dyn serialport::SerialPort>,
    pub software_id: String,
    pub version: (u8, u8),
    pub buffer_size: u16,
}

impl Bootloader {
    pub fn open(path: &str) -> Result<Self> {
        let port = serialport::new(path, 57_600)
            .timeout(Duration::from_millis(50))
            .open()?;
        let mut bl = Self {
            port,
            software_id: String::new(),
            version: (0, 0),
            buffer_size: 0,
        };
        bl.port.clear(serialport::ClearBuffer::All)?;
        bl.software_id = String::from_utf8_lossy(&bl.cmd(b"S", 7)?).into_owned();
        let v = bl.cmd(b"V", 2)?;
        bl.version = (v[0], v[1]);
        let b = bl.cmd(b"b", 3)?;
        if b[0] != b'Y' {
            return Err(FirmwareError::Other(
                "bootloader has no block-mode support".into(),
            ));
        }
        bl.buffer_size = u16::from_be_bytes([b[1], b[2]]);
        debug!(id = bl.software_id, version = ?bl.version, buffer = bl.buffer_size, "bootloader identified");
        Ok(bl)
    }

    fn read_exact_timeout(&mut self, buf: &mut [u8], timeout: Duration, cmd: char) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut got = 0;
        while got < buf.len() {
            if Instant::now() > deadline {
                return Err(FirmwareError::Timeout(cmd));
            }
            match self.port.read(&mut buf[got..]) {
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Send a command and read a fixed-length reply.
    fn cmd(&mut self, bytes: &[u8], reply_len: usize) -> Result<Vec<u8>> {
        self.port.write_all(bytes)?;
        let mut reply = vec![0u8; reply_len];
        self.read_exact_timeout(&mut reply, Duration::from_secs(5), bytes[0] as char)?;
        Ok(reply)
    }

    /// Send a command that must be acknowledged with '\r'.
    fn cmd_ack(&mut self, bytes: &[u8]) -> Result<()> {
        let r = self.cmd(bytes, 1)?;
        if r != b"\r" {
            return Err(FirmwareError::Protocol {
                cmd: bytes[0] as char,
                expected: "\\r".into(),
                got: r,
            });
        }
        Ok(())
    }

    fn set_address(&mut self, byte_addr: u32) -> Result<()> {
        let word = byte_addr / 2;
        let [_, _, hi, lo] = word.to_be_bytes();
        self.cmd_ack(&[b'A', hi, lo])
    }

    /// Read `len` bytes of flash starting at `start` (auto-incrementing address).
    pub fn read_flash(
        &mut self,
        start: u32,
        len: u32,
        mut progress: impl FnMut(u32),
    ) -> Result<Vec<u8>> {
        self.set_address(start)?;
        let mut out = Vec::with_capacity(len as usize);
        let chunk = u32::from(self.buffer_size.max(1)).min(256);
        let mut done = 0;
        while done < len {
            let n = chunk.min(len - done);
            self.port
                .write_all(&[b'g', n.to_be_bytes()[2], n.to_be_bytes()[3], b'F'])?;
            let mut buf = vec![0u8; n as usize];
            self.read_exact_timeout(&mut buf, Duration::from_secs(5), 'g')?;
            out.extend(buf);
            done += n;
            progress(done);
        }
        Ok(out)
    }

    pub fn enter_programming(&mut self) -> Result<()> {
        self.cmd_ack(b"P")
    }

    pub fn leave_programming(&mut self) -> Result<()> {
        self.cmd_ack(b"L")
    }

    /// Erase the application section (Caterina never erases its own section).
    pub fn chip_erase(&mut self) -> Result<()> {
        self.port.write_all(b"e")?;
        let mut r = [0u8; 1];
        self.read_exact_timeout(&mut r, Duration::from_secs(20), 'e')?;
        if r != *b"\r" {
            return Err(FirmwareError::Protocol {
                cmd: 'e',
                expected: "\\r".into(),
                got: r.to_vec(),
            });
        }
        Ok(())
    }

    /// Write `data` starting at `start`, in bootloader-buffer-sized blocks.
    pub fn write_flash(
        &mut self,
        start: u32,
        data: &[u8],
        mut progress: impl FnMut(u32),
    ) -> Result<()> {
        self.set_address(start)?;
        let chunk = usize::from(self.buffer_size.max(1));
        let mut done = 0usize;
        for block in data.chunks(chunk) {
            let len = u16::try_from(block.len()).unwrap_or(u16::MAX).to_be_bytes();
            let mut msg = vec![b'B', len[0], len[1], b'F'];
            msg.extend_from_slice(block);
            self.port.write_all(&msg)?;
            let mut r = [0u8; 1];
            self.read_exact_timeout(&mut r, Duration::from_secs(5), 'B')?;
            if r != *b"\r" {
                return Err(FirmwareError::Protocol {
                    cmd: 'B',
                    expected: "\\r".into(),
                    got: r.to_vec(),
                });
            }
            done += block.len();
            progress(u32::try_from(done).unwrap_or(u32::MAX));
        }
        Ok(())
    }

    /// Exit the bootloader and start the application.
    pub fn exit(mut self) -> Result<()> {
        self.port.write_all(b"E")?;
        let mut r = [0u8; 1];
        let _ = self.read_exact_timeout(&mut r, Duration::from_secs(2), 'E');
        info!("bootloader exited; application starting");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let mut img = Image::default();
        for a in 0..40u32 {
            img.bytes.insert(a, u8::try_from(a * 3).unwrap_or(0));
        }
        let hex = img.to_intel_hex();
        let back = Image::parse_intel_hex(&hex).expect("parse");
        assert_eq!(back.flat(0, 32), img.flat(0, 32));
        assert_eq!(back.max_addr(), Some(31)); // partial last chunk is dropped by the writer
    }

    #[test]
    fn rejects_bad_checksum() {
        assert!(Image::parse_intel_hex(":0100000001FF\n").is_err());
        assert!(Image::parse_intel_hex(":00000001FF\n")
            .expect("eof only")
            .bytes
            .is_empty());
    }

    #[test]
    fn extended_linear_address() {
        let img =
            Image::parse_intel_hex(":020000040001F9\n:0100000042BD\n:00000001FF\n").expect("parse");
        assert_eq!(img.bytes.get(&0x10000), Some(&0x42));
    }
}
