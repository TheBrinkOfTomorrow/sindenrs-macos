//! macOS discovery through the IOKit registry.
//!
//! Every USB device is an `IOUSBHostDevice` with `idVendor`, `idProduct` and a `locationID`
//! that encodes where it sits on the bus. A gun's CDC port shows up below its device as an
//! `IOSerialBSDClient` whose `IOCalloutDevice` is the `/dev/cu.*` node. The location is rendered
//! in the Linux `bus-port.port` form so [`GunDevice::sibling_camera`] works unchanged: the gun
//! and its camera hang off the same hub inside the gun.

#![allow(unsafe_code)]

use std::path::PathBuf;

use core_foundation::base::{kCFAllocatorDefault, CFType, TCFType};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use io_kit_sys::keys::kIOServicePlane;
use io_kit_sys::types::{io_iterator_t, io_object_t};
use io_kit_sys::{
    kIORegistryIterateRecursively, IOIteratorNext, IOObjectRelease,
    IORegistryEntryCreateCFProperty, IORegistryEntrySearchCFProperty, IOServiceGetMatchingServices,
    IOServiceMatching,
};

use super::{CameraDevice, GunDevice};
use crate::ids::{self, GunVariant};

/// An IOKit object reference, released on drop.
pub(crate) struct Object(io_object_t);

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: we own one reference to a valid object.
        unsafe { IOObjectRelease(self.0) };
    }
}

impl Object {
    /// The raw object, borrowed.
    pub(crate) const fn raw(&self) -> io_object_t {
        self.0
    }

    /// A property of this entry.
    fn property(&self, key: &str) -> Option<CFType> {
        let key = CFString::new(key);
        // SAFETY: valid entry and key; the result follows the Create rule.
        let r = unsafe {
            IORegistryEntryCreateCFProperty(
                self.0,
                key.as_concrete_TypeRef(),
                kCFAllocatorDefault,
                0,
            )
        };
        // SAFETY: a non-null result is an owned CF object.
        (!r.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(r) })
    }

    /// A property of this entry or, failing that, of any entry below it.
    fn search(&self, key: &str) -> Option<CFType> {
        let key = CFString::new(key);
        // SAFETY: valid entry, plane name and key; the result follows the Create rule.
        let r = unsafe {
            IORegistryEntrySearchCFProperty(
                self.0,
                kIOServicePlane,
                key.as_concrete_TypeRef(),
                kCFAllocatorDefault,
                kIORegistryIterateRecursively,
            )
        };
        // SAFETY: a non-null result is an owned CF object.
        (!r.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(r) })
    }

    fn string(&self, key: &str) -> Option<String> {
        self.property(key)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.property(key)?.downcast::<CFNumber>()?.to_i64()
    }
}

/// Every registered USB device.
fn usb_devices() -> std::io::Result<Vec<Object>> {
    let class = c"IOUSBHostDevice";
    // SAFETY: a nul-terminated class name. The returned dictionary is consumed by
    // IOServiceGetMatchingServices, so it is not released here.
    let matching = unsafe { IOServiceMatching(class.as_ptr()) };
    if matching.is_null() {
        return Err(std::io::Error::other("IOServiceMatching failed"));
    }
    let mut iter: io_iterator_t = 0;
    // SAFETY: port 0 is the default main port; `matching` is a valid dictionary.
    let kr = unsafe { IOServiceGetMatchingServices(0, matching.cast_const(), &mut iter) };
    if kr != 0 {
        return Err(std::io::Error::other(format!(
            "IOServiceGetMatchingServices: {kr:#x}"
        )));
    }
    let iter = Object(iter);
    let mut out = Vec::new();
    loop {
        // SAFETY: a valid iterator; each returned object is owned by the caller.
        let o = unsafe { IOIteratorNext(iter.0) };
        if o == 0 {
            break;
        }
        out.push(Object(o));
    }
    Ok(out)
}

struct Usb {
    obj: Object,
    vid: u16,
    pid: u16,
    location: u32,
}

fn usb_with_ids() -> std::io::Result<Vec<Usb>> {
    Ok(usb_devices()?
        .into_iter()
        .filter_map(|obj| {
            let vid = u16::try_from(obj.int("idVendor")?).ok()?;
            let pid = u16::try_from(obj.int("idProduct")?).ok()?;
            let location = u32::try_from(obj.int("locationID")?).ok()?;
            Some(Usb {
                obj,
                vid,
                pid,
                location,
            })
        })
        .collect())
}

/// The USB device at `location` with the given IDs.
pub(crate) fn usb_device_at(location: u32, vid: u16, pid: u16) -> std::io::Result<Option<Object>> {
    Ok(usb_with_ids()?
        .into_iter()
        .find(|d| d.location == location && d.vid == vid && d.pid == pid)
        .map(|d| d.obj))
}

/// The inverse of [`avfoundation_id`]: location, VID and PID from a camera `uniqueID`.
pub fn parse_avfoundation_id(id: &str) -> Option<(u32, u16, u16)> {
    let hex = id.strip_prefix("0x")?;
    if hex.len() <= 8 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let (loc, ids) = hex.split_at(hex.len() - 8);
    Some((
        u32::from_str_radix(loc, 16).ok()?,
        u16::from_str_radix(&ids[..4], 16).ok()?,
        u16::from_str_radix(&ids[4..], 16).ok()?,
    ))
}

/// `locationID` as a Linux-style topology path: the top byte is the bus, then one nibble per
/// hub level, ending at the first zero nibble. `0x08342000` is `8-3.4.2`.
pub fn location_path(location: u32) -> String {
    let bus = location >> 24;
    let ports: Vec<String> = (0..6)
        .map(|i| (location >> (20 - 4 * i)) & 0xf)
        .take_while(|&n| n != 0)
        .map(|n| n.to_string())
        .collect();
    format!("{bus}-{}", ports.join("."))
}

/// The `uniqueID` AVFoundation gives a USB camera: the location ID in hex, then VID and PID.
pub fn avfoundation_id(location: u32, vid: u16, pid: u16) -> String {
    format!("{location:#x}{vid:04x}{pid:04x}")
}

fn callout_device(dev: &Usb) -> Option<PathBuf> {
    dev.obj
        .search("IOCalloutDevice")?
        .downcast::<CFString>()
        .map(|s| PathBuf::from(s.to_string()))
}

pub fn find_guns() -> std::io::Result<Vec<GunDevice>> {
    let mut out: Vec<GunDevice> = usb_with_ids()?
        .into_iter()
        .filter(|d| ids::is_gun(d.vid, d.pid))
        .filter_map(|d| {
            Some(GunDevice {
                port: callout_device(&d)?,
                vid: d.vid,
                pid: d.pid,
                variant: GunVariant::from_pid(d.pid),
                product: d.obj.string("USB Product Name"),
                serial: d.obj.string("USB Serial Number"),
                usb_path: location_path(d.location),
            })
        })
        .collect();
    out.sort_by(|a, b| a.port.cmp(&b.port));
    Ok(out)
}

pub fn find_ttys_by_ids(vid: u16, pid: u16) -> std::io::Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = usb_with_ids()?
        .iter()
        .filter(|d| d.vid == vid && d.pid == pid)
        .filter_map(callout_device)
        .collect();
    out.sort();
    Ok(out)
}

/// Cameras by USB identity. `node` is the AVFoundation `uniqueID`, which is what capture opens.
pub fn find_cameras() -> std::io::Result<Vec<CameraDevice>> {
    let mut out: Vec<CameraDevice> = usb_with_ids()?
        .into_iter()
        .filter(|d| ids::is_camera(d.vid, d.pid))
        .map(|d| CameraDevice {
            node: PathBuf::from(avfoundation_id(d.location, d.vid, d.pid)),
            name: d.obj.string("USB Product Name").unwrap_or_default(),
            vid: d.vid,
            pid: d.pid,
            usb_path: location_path(d.location),
            is_capture: Some(true),
        })
        .collect();
    out.sort_by(|a, b| a.usb_path.cmp(&b.usb_path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_paths() {
        assert_eq!(location_path(0x0834_2000), "8-3.4.2");
        assert_eq!(location_path(0x0834_1000), "8-3.4.1");
        assert_eq!(location_path(0x0010_0000), "0-1");
        assert_eq!(location_path(0x1412_3456), "20-1.2.3.4.5.6");
    }

    #[test]
    fn avfoundation_unique_id() {
        // Observed on hardware for the camera at 0x08341000.
        assert_eq!(
            avfoundation_id(0x0834_1000, 0x32e4, 0x9210),
            "0x834100032e49210"
        );
    }

    #[test]
    fn avfoundation_id_round_trips() {
        assert_eq!(
            parse_avfoundation_id("0x834100032e49210"),
            Some((0x0834_1000, 0x32e4, 0x9210))
        );
        assert_eq!(parse_avfoundation_id("Camo"), None);
        assert_eq!(parse_avfoundation_id("0x32e49210"), None);
    }

    #[test]
    fn gun_pairs_with_camera_on_same_hub() {
        let gun = GunDevice {
            port: "/dev/cu.usbmodemHIDDO1".into(),
            vid: 0x16c0,
            pid: 0x0f01,
            variant: None,
            product: None,
            serial: None,
            usb_path: location_path(0x0834_2000),
        };
        let cam = |loc| CameraDevice {
            node: avfoundation_id(loc, 0x32e4, 0x9210).into(),
            name: String::new(),
            vid: 0x32e4,
            pid: 0x9210,
            usb_path: location_path(loc),
            is_capture: Some(true),
        };
        let cams = [cam(0x0835_1000), cam(0x0834_1000)];
        let found = gun.sibling_camera(&cams).expect("paired");
        assert_eq!(found.usb_path, "8-3.4.1");
    }
}
