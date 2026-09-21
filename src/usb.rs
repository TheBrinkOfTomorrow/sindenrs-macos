//! USB housekeeping: power-cycling a gun through its own internal hub.
//!
//! The gun contains a Microchip USB2512 hub with per-port power switching; the microcontroller
//! hangs off one port and the camera off the other. The gun firmware stops servicing its serial
//! port permanently if it ever receives a malformed frame (observed on firmware 1.5), and a USB
//! bus reset does not reboot the microcontroller, so a real power cycle is the only recovery
//! short of unplugging. This does what `uhubctl -a cycle` does, natively.

#[cfg(target_os = "linux")]
mod linux {
    #![allow(unsafe_code)]

    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use tracing::{debug, info};

    const USB_REQ_CLEAR_FEATURE: u8 = 1;
    const USB_REQ_SET_FEATURE: u8 = 3;
    const USB_PORT_FEAT_POWER: u16 = 8;
    /// bmRequestType: host-to-device, class, recipient = other (a hub port).
    const RT_HUB_PORT_OUT: u8 = 0x23;
    /// `_IOWR('U', 0, struct usbdevfs_ctrltransfer)` — 24 bytes on 64-bit.
    const USBDEVFS_CONTROL: u64 = 0xC018_5500;

    #[repr(C)]
    struct CtrlTransfer {
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        timeout_ms: u32,
        data: *mut libc::c_void,
    }

    /// Where a device sits: its parent hub's usbfs node and the port number on that hub.
    #[derive(Clone, Debug)]
    pub struct HubPort {
        pub hub_sysfs: PathBuf,
        pub hub_node: PathBuf,
        pub port: u16,
    }

    /// Resolve the parent hub and port of a device given its sysfs bus path (e.g. `1-3.2`).
    pub fn hub_port_of(usb_path: &str) -> io::Result<HubPort> {
        let (parent, port) = match usb_path.rsplit_once('.') {
            Some((p, port)) => (p.to_owned(), port),
            None => match usb_path.split_once('-') {
                // Directly on a root hub: "1-3" -> hub "usb1", port 3.
                Some((bus, port)) => (format!("usb{bus}"), port),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "not a USB bus path",
                    ))
                }
            },
        };
        let port: u16 = port
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad port number"))?;
        let hub_sysfs = PathBuf::from("/sys/bus/usb/devices").join(&parent);
        let read = |n: &str| -> io::Result<u32> {
            std::fs::read_to_string(hub_sysfs.join(n))?
                .trim()
                .parse()
                .map_err(|_| io::Error::other(format!("bad {n}")))
        };
        let (bus, dev) = (read("busnum")?, read("devnum")?);
        Ok(HubPort {
            hub_sysfs,
            hub_node: PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}")),
            port,
        })
    }

    fn port_feature(hub: &std::fs::File, request: u8, port: u16) -> io::Result<()> {
        let mut xfer = CtrlTransfer {
            request_type: RT_HUB_PORT_OUT,
            request,
            value: USB_PORT_FEAT_POWER,
            index: port,
            length: 0,
            timeout_ms: 1000,
            data: std::ptr::null_mut(),
        };
        // SAFETY: xfer is a correctly laid out usbdevfs_ctrltransfer with no data stage.
        let r = unsafe { libc::ioctl(hub.as_raw_fd(), USBDEVFS_CONTROL as _, &mut xfer) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Hub-class GET_STATUS for a port: returns `wPortStatus` (bit 0 = connection, bit 8 = power).
    pub fn port_status(hub: &std::fs::File, port: u16) -> io::Result<u16> {
        let mut buf = [0u8; 4];
        let mut xfer = CtrlTransfer {
            request_type: 0xA3, // device-to-host, class, other
            request: 0,         // GET_STATUS
            value: 0,
            index: port,
            length: 4,
            timeout_ms: 1000,
            data: buf.as_mut_ptr().cast(),
        };
        // SAFETY: buf outlives the call and is exactly `length` bytes.
        let r = unsafe { libc::ioctl(hub.as_raw_fd(), USBDEVFS_CONTROL as _, &mut xfer) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u16::from_le_bytes([buf[0], buf[1]]))
    }

    const PORT_STAT_CONNECTION: u16 = 1 << 0;
    const PORT_STAT_POWER: u16 = 1 << 8;

    fn wait_status(
        hub: &std::fs::File,
        port: u16,
        pred: impl Fn(u16) -> bool,
        timeout: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            let st = port_status(hub, port)?;
            if pred(st) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                debug!(
                    status = format!("{st:#06x}"),
                    "port status did not reach the expected state"
                );
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The sysfs attribute (Linux >= 6.0) that disables a hub port cooperatively with the kernel.
    fn disable_attr(hp: &HubPort) -> PathBuf {
        let hub = hp
            .hub_sysfs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        hp.hub_sysfs
            .join(format!("{hub}:1.0"))
            .join(format!("{hub}-port{}", hp.port))
            .join("disable")
    }

    /// Cut power to `port` on the hub for `off_for`, then restore it.
    ///
    /// Preferred path: the port's sysfs `disable` attribute. Sending CLEAR_FEATURE(PORT_POWER)
    /// straight to the hub also works electrically, but the kernel hub driver notices the port
    /// losing power and turns it back on within about a second — too short to drain the gun's
    /// recoil capacitors, so the MCU never resets. The sysfs route tells the kernel it was on
    /// purpose. The raw request remains as a fallback for old kernels.
    pub fn power_cycle(hp: &HubPort, off_for: Duration) -> io::Result<()> {
        let attr = disable_attr(hp);
        if attr.exists() {
            info!(port = %attr.display(), "power-cycling port via sysfs");
            let write = |v: &str| {
                std::fs::write(&attr, v).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "{}: {e} (the udev rules make this writable for the dialout group)",
                            attr.display()
                        ),
                    )
                })
            };
            write("1")?;
            std::thread::sleep(off_for);
            write("0")?;
            return Ok(());
        }
        power_cycle_raw(hp, off_for)
    }

    /// Fallback: hub-class feature requests through usbfs, verified through port status.
    pub fn power_cycle_raw(hp: &HubPort, off_for: Duration) -> io::Result<()> {
        let hub = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&hp.hub_node)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "{}: {e} (the udev rules grant access to the gun's hub)",
                        hp.hub_node.display()
                    ),
                )
            })?;
        let before = port_status(&hub, hp.port)?;
        info!(hub = %hp.hub_node.display(), port = hp.port, status = format!("{before:#06x}"), "power-cycling port via hub requests");
        let mut off = false;
        for attempt in 1..=5 {
            port_feature(&hub, USB_REQ_CLEAR_FEATURE, hp.port)?;
            if wait_status(
                &hub,
                hp.port,
                |s| s & (PORT_STAT_POWER | PORT_STAT_CONNECTION) == 0,
                Duration::from_millis(1500),
            )? {
                off = true;
                break;
            }
            debug!(
                attempt,
                "port still powered after CLEAR_FEATURE(PORT_POWER); retrying"
            );
        }
        if !off {
            return Err(io::Error::other(
                "hub did not power the port off (no per-port power switching?)",
            ));
        }
        std::thread::sleep(off_for);
        port_feature(&hub, USB_REQ_SET_FEATURE, hp.port)?;
        if !wait_status(
            &hub,
            hp.port,
            |s| s & PORT_STAT_CONNECTION != 0,
            Duration::from_secs(5),
        )? {
            return Err(io::Error::other(
                "device did not reconnect after power was restored",
            ));
        }
        let after = port_status(&hub, hp.port)?;
        debug!(
            status = format!("{after:#06x}"),
            "port powered and connected again"
        );
        Ok(())
    }

    /// Wait until `path` exists again, or `timeout` elapses. Deliberately does not open it:
    /// opening a CDC-ACM port toggles DTR, and doing that while the firmware is still booting
    /// has been seen to leave it unresponsive.
    pub fn wait_for_node(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.exists() {
                debug!(path = %path.display(), "device node is back");
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

#[cfg(target_os = "linux")]
pub use linux::{hub_port_of, power_cycle, wait_for_node, HubPort};

#[cfg(not(target_os = "linux"))]
pub fn power_cycle_unsupported() -> std::io::Error {
    std::io::Error::other("power-cycling the gun is not implemented on this platform yet")
}
