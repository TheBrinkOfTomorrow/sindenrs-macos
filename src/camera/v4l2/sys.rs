//! Hand-written V4L2 ABI: the structs and ioctl numbers this driver uses.
//!
//! These are copied from `<linux/videodev2.h>` and checked by tests against the ioctl
//! numbers the kernel actually produces on x86-64/aarch64 (the size is encoded in the ioctl
//! number, so a wrong layout fails the test rather than the camera). Kept minimal on purpose:
//! doing it this way avoids a bindgen/libclang build dependency that would otherwise have to
//! exist on every build host, including the Nix sandbox and the Windows cross build.

#![allow(non_camel_case_types, dead_code)]

pub const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const V4L2_MEMORY_MMAP: u32 = 1;
pub const V4L2_FIELD_ANY: u32 = 0;
pub const V4L2_FIELD_NONE: u32 = 1;

pub const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
pub const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

pub const V4L2_BUF_FLAG_ERROR: u32 = 0x0000_0040;
pub const V4L2_BUF_FLAG_TIMESTAMP_MASK: u32 = 0x0000_e000;
pub const V4L2_BUF_FLAG_TIMESTAMP_UNKNOWN: u32 = 0x0000_0000;
pub const V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x0000_2000;
pub const V4L2_BUF_FLAG_TIMESTAMP_COPY: u32 = 0x0000_4000;

pub const V4L2_CTRL_FLAG_DISABLED: u32 = 0x0001;
pub const V4L2_CTRL_FLAG_INACTIVE: u32 = 0x0010;
pub const V4L2_CTRL_FLAG_NEXT_CTRL: u32 = 0x8000_0000;

pub const V4L2_CTRL_TYPE_INTEGER: u32 = 1;
pub const V4L2_CTRL_TYPE_BOOLEAN: u32 = 2;
pub const V4L2_CTRL_TYPE_MENU: u32 = 3;
pub const V4L2_CTRL_TYPE_CTRL_CLASS: u32 = 6;
pub const V4L2_CTRL_TYPE_INTEGER_MENU: u32 = 9;

pub const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
pub const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;

pub const V4L2_CAP_TIMEPERFRAME: u32 = 0x1000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_pix_format {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

/// `union { v4l2_pix_format pix; ...; u8 raw_data[200]; }` — 8-byte aligned because the
/// overlay window member holds pointers.
#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_format_union {
    pub pix: v4l2_pix_format,
    pub raw_data: [u8; 200],
    _align: [u64; 25],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_format {
    pub type_: u32,
    pub fmt: v4l2_format_union,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_requestbuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_buffer_m {
    pub offset: u32,
    pub userptr: u64,
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: timeval,
    pub timecode: v4l2_timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: v4l2_buffer_m,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

impl v4l2_buffer {
    pub fn zeroed_capture(index: u32) -> Self {
        Self {
            index,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            bytesused: 0,
            flags: 0,
            field: 0,
            timestamp: timeval::default(),
            timecode: v4l2_timecode::default(),
            sequence: 0,
            memory: V4L2_MEMORY_MMAP,
            m: v4l2_buffer_m { userptr: 0 },
            length: 0,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_control {
    pub id: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_queryctrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
    pub flags: u32,
    pub reserved: [u32; 2],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct v4l2_querymenu {
    pub id: u32,
    pub index: u32,
    /// `union { u8 name[32]; i64 value; }`
    pub name: [u8; 32],
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_fmtdesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_frmsize_discrete {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_frmsize_stepwise {
    pub min_width: u32,
    pub max_width: u32,
    pub step_width: u32,
    pub min_height: u32,
    pub max_height: u32,
    pub step_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_frmsize_union {
    pub discrete: v4l2_frmsize_discrete,
    pub stepwise: v4l2_frmsize_stepwise,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_frmsizeenum {
    pub index: u32,
    pub pixel_format: u32,
    pub type_: u32,
    pub u: v4l2_frmsize_union,
    pub reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_fract {
    pub numerator: u32,
    pub denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_frmival_stepwise {
    pub min: v4l2_fract,
    pub max: v4l2_fract,
    pub step: v4l2_fract,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_frmival_union {
    pub discrete: v4l2_fract,
    pub stepwise: v4l2_frmival_stepwise,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_frmivalenum {
    pub index: u32,
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub type_: u32,
    pub u: v4l2_frmival_union,
    pub reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct v4l2_captureparm {
    pub capability: u32,
    pub capturemode: u32,
    pub timeperframe: v4l2_fract,
    pub extendedmode: u32,
    pub readbuffers: u32,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union v4l2_streamparm_union {
    pub capture: v4l2_captureparm,
    pub raw_data: [u8; 200],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct v4l2_streamparm {
    pub type_: u32,
    pub parm: v4l2_streamparm_union,
}

// ioctl number construction (asm-generic, which x86-64 and aarch64 both use).
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation)] // struct sizes are far below 2^14
    let size = size as u32;
    (dir << IOC_DIRSHIFT)
        | ((b'V' as u32) << IOC_TYPESHIFT)
        | (size << IOC_SIZESHIFT)
        | (nr << IOC_NRSHIFT)
}
const fn ior<T>(nr: u32) -> u32 {
    ioc(IOC_READ, nr, core::mem::size_of::<T>())
}
const fn iow<T>(nr: u32) -> u32 {
    ioc(IOC_WRITE, nr, core::mem::size_of::<T>())
}
const fn iowr<T>(nr: u32) -> u32 {
    ioc(IOC_READ | IOC_WRITE, nr, core::mem::size_of::<T>())
}

pub const VIDIOC_QUERYCAP: u32 = ior::<v4l2_capability>(0);
pub const VIDIOC_ENUM_FMT: u32 = iowr::<v4l2_fmtdesc>(2);
pub const VIDIOC_G_FMT: u32 = iowr::<v4l2_format>(4);
pub const VIDIOC_S_FMT: u32 = iowr::<v4l2_format>(5);
pub const VIDIOC_REQBUFS: u32 = iowr::<v4l2_requestbuffers>(8);
pub const VIDIOC_QUERYBUF: u32 = iowr::<v4l2_buffer>(9);
pub const VIDIOC_QBUF: u32 = iowr::<v4l2_buffer>(15);
pub const VIDIOC_DQBUF: u32 = iowr::<v4l2_buffer>(17);
pub const VIDIOC_STREAMON: u32 = iow::<i32>(18);
pub const VIDIOC_STREAMOFF: u32 = iow::<i32>(19);
pub const VIDIOC_G_PARM: u32 = iowr::<v4l2_streamparm>(21);
pub const VIDIOC_S_PARM: u32 = iowr::<v4l2_streamparm>(22);
pub const VIDIOC_G_CTRL: u32 = iowr::<v4l2_control>(27);
pub const VIDIOC_S_CTRL: u32 = iowr::<v4l2_control>(28);
pub const VIDIOC_QUERYCTRL: u32 = iowr::<v4l2_queryctrl>(36);
pub const VIDIOC_QUERYMENU: u32 = iowr::<v4l2_querymenu>(37);
pub const VIDIOC_ENUM_FRAMESIZES: u32 = iowr::<v4l2_frmsizeenum>(74);
pub const VIDIOC_ENUM_FRAMEINTERVALS: u32 = iowr::<v4l2_frmivalenum>(75);

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    /// Values observed in the stock native library's disassembly and in the kernel headers.
    #[test]
    fn ioctl_numbers_match_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_G_PARM, 0xc0cc_5615);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
        assert_eq!(VIDIOC_QUERYCTRL, 0xc044_5624);
        assert_eq!(VIDIOC_QUERYMENU, 0xc02c_5625);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
    }

    #[test]
    fn struct_sizes() {
        assert_eq!(size_of::<v4l2_capability>(), 104);
        assert_eq!(size_of::<v4l2_pix_format>(), 48);
        assert_eq!(size_of::<v4l2_format>(), 208);
        assert_eq!(size_of::<v4l2_requestbuffers>(), 20);
        assert_eq!(size_of::<v4l2_buffer>(), 88);
        assert_eq!(size_of::<v4l2_control>(), 8);
        assert_eq!(size_of::<v4l2_queryctrl>(), 68);
        assert_eq!(size_of::<v4l2_querymenu>(), 44);
        assert_eq!(size_of::<v4l2_fmtdesc>(), 64);
        assert_eq!(size_of::<v4l2_frmsizeenum>(), 44);
        assert_eq!(size_of::<v4l2_frmivalenum>(), 52);
        assert_eq!(size_of::<v4l2_streamparm>(), 204);
        // Field offsets that matter for reading timestamps back.
        assert_eq!(core::mem::offset_of!(v4l2_buffer, timestamp), 24);
        assert_eq!(core::mem::offset_of!(v4l2_buffer, sequence), 56);
        assert_eq!(core::mem::offset_of!(v4l2_buffer, m), 64);
        assert_eq!(core::mem::offset_of!(v4l2_buffer, length), 72);
    }
}
