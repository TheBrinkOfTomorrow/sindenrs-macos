//! UVC control requests on macOS, over the USB device's default control pipe.
//!
//! Apple's UVC driver owns the camera while AVFoundation streams, but class requests on the
//! default pipe (`DeviceRequest` without `USBDeviceOpen`) are still delivered and take effect
//! mid-stream (docs/macos-notes.md). The IOKit USB user-client interface is a COM-style vtable
//! with no Rust bindings; only the slots up to `DeviceRequest` are declared here, in the order
//! of `IOUSBDeviceStruct100` in `IOUSBLib.h`.

#![allow(unsafe_code, non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::ptr;

use core_foundation_sys::base::CFAllocatorRef;
use core_foundation_sys::uuid::{
    CFUUIDBytes, CFUUIDGetConstantUUIDWithBytes, CFUUIDGetUUIDBytes, CFUUIDRef,
};
use io_kit_sys::types::io_service_t;

use super::{
    ae_mode_from_v4l2, ae_mode_to_v4l2, decode, encode, req, selector, Selector, Topology,
};
use crate::camera::cid;
use crate::discovery::macos::{parse_avfoundation_id, usb_device_at};

type IOReturn = i32;
type HRESULT = i32;
type Unused = *const c_void;

/// `IUNKNOWN_C_GUTS`.
#[repr(C)]
struct IUnknownVtbl {
    _reserved: Unused,
    QueryInterface: unsafe extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    AddRef: unsafe extern "C" fn(*mut c_void) -> u32,
    Release: unsafe extern "C" fn(*mut c_void) -> u32,
}

/// `IOUSBDevRequest`.
#[repr(C)]
struct IOUSBDevRequest {
    bmRequestType: u8,
    bRequest: u8,
    wValue: u16,
    wIndex: u16,
    wLength: u16,
    pData: *mut c_void,
    wLenDone: u32,
}

/// `IOUSBConfigurationDescriptor`'s leading fields.
#[repr(C, packed)]
struct ConfigHeader {
    bLength: u8,
    bDescriptorType: u8,
    wTotalLength: u16,
}

/// `IOUSBDeviceStruct100`, through `DeviceRequest`.
#[repr(C)]
struct DeviceVtbl {
    base: IUnknownVtbl,
    CreateDeviceAsyncEventSource: Unused,
    GetDeviceAsyncEventSource: Unused,
    CreateDeviceAsyncPort: Unused,
    GetDeviceAsyncPort: Unused,
    USBDeviceOpen: Unused,
    USBDeviceClose: Unused,
    GetDeviceClass: Unused,
    GetDeviceSubClass: Unused,
    GetDeviceProtocol: Unused,
    GetDeviceVendor: Unused,
    GetDeviceProduct: Unused,
    GetDeviceReleaseNumber: Unused,
    GetDeviceAddress: Unused,
    GetDeviceBusPowerAvailable: Unused,
    GetDeviceSpeed: Unused,
    GetNumberOfConfigurations: Unused,
    GetLocationID: Unused,
    GetConfigurationDescriptorPtr:
        unsafe extern "C" fn(*mut c_void, u8, *mut *const ConfigHeader) -> IOReturn,
    GetConfiguration: Unused,
    SetConfiguration: Unused,
    GetBusFrameNumber: Unused,
    ResetDevice: Unused,
    DeviceRequest: unsafe extern "C" fn(*mut c_void, *mut IOUSBDevRequest) -> IOReturn,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOCreatePlugInInterfaceForService(
        service: io_service_t,
        plugin_type: CFUUIDRef,
        interface_type: CFUUIDRef,
        the_interface: *mut *mut *mut IUnknownVtbl,
        the_score: *mut i32,
    ) -> i32;
}

fn uuid(b: [u8; 16]) -> CFUUIDRef {
    // SAFETY: constant UUIDs are interned and never released.
    unsafe {
        CFUUIDGetConstantUUIDWithBytes(
            ptr::null::<c_void>() as CFAllocatorRef,
            b[0],
            b[1],
            b[2],
            b[3],
            b[4],
            b[5],
            b[6],
            b[7],
            b[8],
            b[9],
            b[10],
            b[11],
            b[12],
            b[13],
            b[14],
            b[15],
        )
    }
}

/// `kIOUSBDeviceUserClientTypeID`.
const USB_DEVICE_USER_CLIENT: [u8; 16] = [
    0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xd4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
];
/// `kIOCFPlugInInterfaceID`.
const CF_PLUGIN_INTERFACE: [u8; 16] = [
    0xc2, 0x44, 0xe8, 0x58, 0x10, 0x9c, 0x11, 0xd4, 0x91, 0xd4, 0x00, 0x50, 0xe4, 0xc6, 0x42, 0x6f,
];
/// `kIOUSBDeviceInterfaceID` (the base interface; `DeviceRequest` is in every version).
const USB_DEVICE_INTERFACE: [u8; 16] = [
    0x5c, 0x81, 0x87, 0xd0, 0x9e, 0xf3, 0x11, 0xd4, 0x8b, 0x45, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61,
];

/// The camera's USB device interface and its VideoControl topology.
pub struct Controls {
    dev: *mut *mut DeviceVtbl,
    topology: Topology,
}

// SAFETY: the IOKit user-client interface may be called from any thread; each request is
// synchronous and independent.
unsafe impl Send for Controls {}

impl std::fmt::Debug for Controls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controls")
            .field("topology", &self.topology)
            .finish_non_exhaustive()
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        // SAFETY: we hold one reference from QueryInterface.
        unsafe { ((**self.dev).base.Release)(self.dev.cast()) };
    }
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

impl Controls {
    /// Open the controls of the camera whose AVFoundation `uniqueID` is `camera` (the node
    /// macOS discovery reports).
    pub fn open(camera: &Path) -> io::Result<Self> {
        let id = camera.to_string_lossy();
        let (location, vid, pid) = parse_avfoundation_id(&id)
            .ok_or_else(|| err(format!("{id} is not a USB camera ID")))?;
        let service = usb_device_at(location, vid, pid)?.ok_or_else(|| {
            err(format!(
                "no USB device {vid:04x}:{pid:04x} at {location:#010x}"
            ))
        })?;

        let mut plugin: *mut *mut IUnknownVtbl = ptr::null_mut();
        let mut score = 0;
        // SAFETY: a valid service and interned UUIDs; on success `plugin` holds one reference.
        let kr = unsafe {
            IOCreatePlugInInterfaceForService(
                service.raw(),
                uuid(USB_DEVICE_USER_CLIENT),
                uuid(CF_PLUGIN_INTERFACE),
                &mut plugin,
                &mut score,
            )
        };
        if kr != 0 || plugin.is_null() {
            return Err(err(format!("IOCreatePlugInInterfaceForService: {kr:#x}")));
        }
        let mut dev: *mut c_void = ptr::null_mut();
        // SAFETY: `plugin` is a valid IUnknown; the IID bytes come from an interned UUID. The
        // plug-in reference is released whether or not the query succeeds.
        let hr = unsafe {
            let hr = ((**plugin).QueryInterface)(
                plugin.cast(),
                CFUUIDGetUUIDBytes(uuid(USB_DEVICE_INTERFACE)),
                &mut dev,
            );
            ((**plugin).Release)(plugin.cast());
            hr
        };
        if hr != 0 || dev.is_null() {
            return Err(err(format!("USB device interface: {hr:#x}")));
        }
        let dev = dev.cast::<*mut DeviceVtbl>();
        // SAFETY: a valid device interface; configuration 0's descriptor stays owned by it.
        let config = unsafe {
            let mut cfg: *const ConfigHeader = ptr::null();
            let kr = ((**dev).GetConfigurationDescriptorPtr)(dev.cast(), 0, &mut cfg);
            if kr != 0 || cfg.is_null() {
                ((**dev).base.Release)(dev.cast());
                return Err(err(format!("configuration descriptor: {kr:#x}")));
            }
            let total = usize::from(u16::from_le(ptr::read_unaligned(ptr::addr_of!(
                (*cfg).wTotalLength
            ))));
            std::slice::from_raw_parts(cfg.cast::<u8>(), total).to_vec()
        };
        let Some(topology) = Topology::parse(&config) else {
            // SAFETY: release the reference taken above.
            unsafe { ((**dev).base.Release)(dev.cast()) };
            return Err(err(
                "no UVC camera terminal and processing unit in the descriptors",
            ));
        };
        Ok(Self { dev, topology })
    }

    pub const fn topology(&self) -> Topology {
        self.topology
    }

    fn request(&self, s: Selector, request: u8, data: &mut [u8]) -> io::Result<()> {
        let (value, index) = self.topology.address(s);
        let mut r = IOUSBDevRequest {
            // Class request to an interface: 0xA1 device-to-host, 0x21 host-to-device.
            bmRequestType: if request & 0x80 != 0 { 0xa1 } else { 0x21 },
            bRequest: request,
            wValue: value,
            wIndex: index,
            wLength: u16::try_from(data.len()).map_err(|_| err("payload too long"))?,
            pData: data.as_mut_ptr().cast(),
            wLenDone: 0,
        };
        // SAFETY: a valid device interface and a request whose buffer outlives the call.
        let kr = unsafe { ((**self.dev).DeviceRequest)(self.dev.cast(), &mut r) };
        if kr == 0 {
            Ok(())
        } else {
            Err(err(format!(
                "UVC request {request:#04x} selector {:#04x}: {kr:#x}",
                s.selector
            )))
        }
    }

    fn mapped(&self, id: u32) -> io::Result<Selector> {
        let s =
            selector(id).ok_or_else(|| err(format!("control {id:#010x} has no UVC mapping")))?;
        if self.topology.supports(s) {
            Ok(s)
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("the camera does not support control {id:#010x}"),
            ))
        }
    }

    fn get_raw(&self, s: Selector, request: u8) -> io::Result<i32> {
        let mut b = [0u8; 4];
        let buf = &mut b[..usize::from(s.len)];
        self.request(s, request, buf)?;
        Ok(decode(buf, s.signed))
    }

    /// A control's current value, in V4L2 terms (as [`crate::camera::v4l2`] would report it).
    pub fn get_control(&self, id: u32) -> io::Result<i32> {
        let s = self.mapped(id)?;
        let v = self.get_raw(s, req::GET_CUR)?;
        Ok(if id == cid::EXPOSURE_AUTO {
            ae_mode_to_v4l2(u8::try_from(v).unwrap_or(0))
        } else {
            v
        })
    }

    /// Set a control, taking V4L2 values.
    pub fn set_control(&self, id: u32, value: i32) -> io::Result<()> {
        let s = self.mapped(id)?;
        let raw = if id == cid::EXPOSURE_AUTO {
            i32::from(
                ae_mode_from_v4l2(value)
                    .ok_or_else(|| err(format!("bad exposure mode {value}")))?,
            )
        } else {
            value
        };
        let mut data = encode(raw, s.len);
        self.request(s, req::SET_CUR, &mut data)
    }

    /// `(min, max, step, default)` of a control, in raw UVC units.
    pub fn range(&self, id: u32) -> io::Result<(i32, i32, i32, i32)> {
        let s = self.mapped(id)?;
        Ok((
            self.get_raw(s, req::GET_MIN)?,
            self.get_raw(s, req::GET_MAX)?,
            self.get_raw(s, req::GET_RES)?,
            self.get_raw(s, req::GET_DEF)?,
        ))
    }

    /// Manual exposure in 100 µs units, as on Linux.
    pub fn set_manual_exposure(&self, units_100us: i32) -> io::Result<()> {
        self.set_control(cid::EXPOSURE_AUTO, cid::EXPOSURE_MANUAL)?;
        self.set_control(cid::EXPOSURE_ABSOLUTE, units_100us)
    }

    /// Back to the camera's automatic exposure (aperture priority, as on Linux).
    pub fn set_auto_exposure(&self) -> io::Result<()> {
        self.set_control(cid::EXPOSURE_AUTO, cid::EXPOSURE_APERTURE_PRIORITY)
    }
}
