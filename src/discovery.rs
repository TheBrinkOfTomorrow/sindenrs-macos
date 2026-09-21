//! Finding guns and cameras.
//!
//! On Linux this walks sysfs directly instead of shelling out to `lsusb`/`udevadm`/`v4l2-ctl`
//! as the stock driver does. Other platforms return nothing until their backends exist.

use std::path::PathBuf;

use crate::ids::GunVariant;

#[derive(Clone, Debug)]
pub struct GunDevice {
    /// Serial device node, e.g. `/dev/ttyACM0` or `COM3`.
    pub port: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub variant: Option<GunVariant>,
    pub product: Option<String>,
    pub serial: Option<String>,
    /// Bus topology, e.g. `1-3.2`. Cameras sharing a hub prefix are likely the same gun.
    pub usb_path: String,
}

#[derive(Clone, Debug)]
pub struct CameraDevice {
    /// Capture device node, e.g. `/dev/video4`.
    pub node: PathBuf,
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    pub usb_path: String,
    /// `Some(true)` if this node is a video capture node (not a metadata node); `None` if it
    /// could not be opened to check.
    pub is_capture: Option<bool>,
}

impl GunDevice {
    /// The camera most likely attached to this gun: same parent hub port.
    pub fn sibling_camera<'a>(&self, cameras: &'a [CameraDevice]) -> Option<&'a CameraDevice> {
        let parent = |p: &str| {
            p.rsplit_once('.')
                .map(|(a, _)| a.to_owned())
                .unwrap_or_else(|| p.to_owned())
        };
        let mine = parent(&self.usb_path);
        cameras
            .iter()
            .filter(|c| c.is_capture != Some(false))
            .find(|c| parent(&c.usb_path) == mine)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::ids;
    use std::fs;
    use std::path::Path;

    fn read_trim(p: &Path) -> Option<String> {
        fs::read_to_string(p).ok().map(|s| s.trim().to_owned())
    }

    fn read_hex(p: &Path) -> Option<u16> {
        read_trim(p).and_then(|s| u16::from_str_radix(&s, 16).ok())
    }

    /// Walk up from a sysfs device path to the USB device (the directory with `idVendor`).
    fn usb_device_dir(mut p: PathBuf) -> Option<PathBuf> {
        for _ in 0..6 {
            if p.join("idVendor").exists() {
                return Some(p);
            }
            p = p.parent()?.to_path_buf();
        }
        None
    }

    /// All CDC-ACM ttys whose USB device matches `vid:pid` (used to find the bootloader).
    pub fn find_ttys_by_ids(vid: u16, pid: u16) -> std::io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/tty") else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("ttyACM") {
                continue;
            }
            let Ok(dev) = fs::canonicalize(e.path().join("device")) else {
                continue;
            };
            let Some(usb) = usb_device_dir(dev) else {
                continue;
            };
            if read_hex(&usb.join("idVendor")) == Some(vid)
                && read_hex(&usb.join("idProduct")) == Some(pid)
            {
                out.push(PathBuf::from(format!("/dev/{name}")));
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn find_guns() -> std::io::Result<Vec<GunDevice>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/tty") else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("ttyACM") {
                continue;
            }
            let Ok(dev) = fs::canonicalize(e.path().join("device")) else {
                continue;
            };
            let Some(usb) = usb_device_dir(dev) else {
                continue;
            };
            let (Some(vid), Some(pid)) = (
                read_hex(&usb.join("idVendor")),
                read_hex(&usb.join("idProduct")),
            ) else {
                continue;
            };
            if !ids::is_gun(vid, pid) {
                continue;
            }
            out.push(GunDevice {
                port: PathBuf::from(format!("/dev/{name}")),
                vid,
                pid,
                variant: GunVariant::from_pid(pid),
                product: read_trim(&usb.join("product")),
                serial: read_trim(&usb.join("serial")),
                usb_path: usb
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            });
        }
        out.sort_by(|a, b| a.port.cmp(&b.port));
        Ok(out)
    }

    pub fn find_cameras() -> std::io::Result<Vec<CameraDevice>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/video4linux") else {
            return Ok(out);
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(dev) = fs::canonicalize(e.path().join("device")) else {
                continue;
            };
            let Some(usb) = usb_device_dir(dev) else {
                continue;
            };
            let (Some(vid), Some(pid)) = (
                read_hex(&usb.join("idVendor")),
                read_hex(&usb.join("idProduct")),
            ) else {
                continue;
            };
            if !ids::is_camera(vid, pid) {
                continue;
            }
            let node = PathBuf::from(format!("/dev/{name}"));
            let is_capture = crate::camera::v4l2::Device::open(&node)
                .and_then(|d| d.capability())
                .ok()
                .map(|c| c.is_video_capture());
            out.push(CameraDevice {
                node,
                name: read_trim(&e.path().join("name")).unwrap_or_default(),
                vid,
                pid,
                usb_path: usb
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                is_capture,
            });
        }
        out.sort_by(|a, b| a.node.cmp(&b.node));
        Ok(out)
    }
}

#[cfg(target_os = "linux")]
pub use linux::{find_cameras, find_guns, find_ttys_by_ids};

#[cfg(not(target_os = "linux"))]
pub fn find_guns() -> std::io::Result<Vec<GunDevice>> {
    // TODO(windows): enumerate COM ports by VID/PID via SetupAPI (serialport::available_ports).
    Ok(Vec::new())
}

#[cfg(not(target_os = "linux"))]
pub fn find_ttys_by_ids(_vid: u16, _pid: u16) -> std::io::Result<Vec<PathBuf>> {
    // TODO(windows): SetupAPI enumeration by VID/PID.
    Ok(Vec::new())
}

#[cfg(not(target_os = "linux"))]
pub fn find_cameras() -> std::io::Result<Vec<CameraDevice>> {
    // TODO(windows): enumerate via Media Foundation.
    Ok(Vec::new())
}
