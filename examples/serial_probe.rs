//! macOS hardware check: handshake with a gun on an explicit serial port, then either sweep the
//! HID pointer in a circle, or (`--buttons`) hold it at screen centre with the default button
//! map and recoil off, and print every change in the button bits.
//!
//! `cargo run --example serial_probe -- /dev/cu.usbmodemXXXX [seconds] [--buttons]`

use std::time::{Duration, Instant};

use sindenrs::config::GunConfig;
use sindenrs::protocol::event::Event;
use sindenrs::protocol::percent_to_axis;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let buttons = args.iter().any(|a| a == "--buttons");
    let mut rest = args.iter().filter(|a| !a.starts_with("--"));
    let port = rest
        .next()
        .expect("usage: serial_probe <port> [seconds] [--buttons]");
    let seconds: f64 = rest.next().map_or(Ok(10.0), |s| s.parse())?;

    let mut gun = sindenrs::gun::Gun::open(port)?;
    let t = Instant::now();
    gun.authenticate()?;
    println!("authenticated in {:?}", t.elapsed());
    let ((maj, min), _) = gun.firmware_version()?;
    println!("firmware {maj}.{min}");
    println!("camera name {:?}", gun.camera_name()?.0);
    println!("unique id {}", gun.unique_id()?);

    if buttons {
        gun.apply_config(&GunConfig::default(), Duration::from_millis(5))?;
    }
    gun.start_streaming(Duration::from_millis(150))?;
    gun.set_offscreen_reload(false)?;
    gun.set_calibration_mode_enabled(false)?;
    gun.set_recoil_enabled(false)?;
    gun.set_secondary_serial(true)?;
    if buttons {
        println!("streaming for {seconds}s, cursor held at centre: press each control in turn");
    } else {
        println!("streaming for {seconds}s: the cursor should circle the screen; pull the trigger");
    }

    let start = Instant::now();
    let mut next = start;
    let period = Duration::from_secs_f64(1.0 / 60.0);
    let mut last = 0u16;
    while start.elapsed().as_secs_f64() < seconds {
        let (x, y) = if buttons {
            (50.0, 50.0)
        } else {
            let a = start.elapsed().as_secs_f64() * std::f64::consts::TAU / 4.0;
            (50.0 + 30.0 * a.cos(), 50.0 + 30.0 * a.sin())
        };
        gun.set_position(percent_to_axis(x), percent_to_axis(y))?;
        for ev in gun.poll_events()? {
            // Wall clock, to line up with other logs (tools/macos/input_window.swift).
            let at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs_f64();
            match ev {
                Event::Buttons {
                    state1,
                    state2,
                    extra,
                } => {
                    let bits = u16::from(state1) | u16::from(state2) << 8;
                    let pressed = bits & !last;
                    let released = last & !bits;
                    println!(
                        "{at:.3}  s1={state1:08b} s2={state2:08b} x={extra:#04x}  down={} up={}",
                        names(pressed),
                        names(released)
                    );
                    last = bits;
                }
                other => println!("{at:.3}  {other:?}"),
            }
        }
        next += period;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    Ok(())
}

/// Bits as `s1.N` / `s2.N`, or `-` for none.
fn names(bits: u16) -> String {
    let v: Vec<String> = (0..16)
        .filter(|b| bits & (1 << b) != 0)
        .map(|b| format!("s{}.{}", b / 8 + 1, b % 8))
        .collect();
    if v.is_empty() {
        "-".into()
    } else {
        v.join(",")
    }
}
