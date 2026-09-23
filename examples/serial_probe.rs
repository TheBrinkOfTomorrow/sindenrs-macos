//! Phase 0 macOS check: handshake with a gun on an explicit serial port, sweep the HID
//! pointer across the screen, and print button events.
//!
//! `cargo run --example serial_probe -- /dev/cu.usbmodemXXXX [seconds]`

use std::time::{Duration, Instant};

use sindenrs::protocol::percent_to_axis;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let port = args.next().expect("usage: serial_probe <port> [seconds]");
    let seconds: f64 = args.next().map_or(Ok(10.0), |s| s.parse())?;

    let mut gun = sindenrs::gun::Gun::open(&port)?;
    let t = Instant::now();
    gun.authenticate()?;
    println!("authenticated in {:?}", t.elapsed());
    let ((maj, min), _) = gun.firmware_version()?;
    println!("firmware {maj}.{min}");
    println!("camera name {:?}", gun.camera_name()?.0);
    println!("unique id {}", gun.unique_id()?);

    gun.start_streaming(Duration::from_millis(150))?;
    gun.set_offscreen_reload(false)?;
    gun.set_calibration_mode_enabled(false)?;
    gun.set_recoil_enabled(false)?;
    gun.set_secondary_serial(true)?;
    println!("streaming for {seconds}s: the cursor should circle the screen; pull the trigger");

    let start = Instant::now();
    let mut next = start;
    let period = Duration::from_secs_f64(1.0 / 60.0);
    while start.elapsed().as_secs_f64() < seconds {
        let a = start.elapsed().as_secs_f64() * std::f64::consts::TAU / 4.0;
        let (x, y) = (50.0 + 30.0 * a.cos(), 50.0 + 30.0 * a.sin());
        gun.set_position(percent_to_axis(x), percent_to_axis(y))?;
        for ev in gun.poll_events()? {
            println!("{:>7.3}s  {ev:?}", start.elapsed().as_secs_f64());
        }
        next += period;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    Ok(())
}
