//! The per-gun tracking loop, shared by `track` (one gun, diagnostics) and `run` (every gun,
//! forever). Capture, decode, acquire, solve, send: all on the calling thread, no queues.

#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::camera::v4l2::Device;
use crate::camera::{cid, PixelFormat};
use crate::config::{Display, Exposure, GunConfig};
use crate::gun::Gun;
use crate::protocol::percent_to_axis;
use crate::vision::acquire::{
    acquire, aim_pixel, flip_luma, flip_point, AcquireParams, Flip, Quad,
};
use crate::vision::luma;

/// What one tracking loop does.
#[derive(Clone, Debug)]
pub struct TrackerOptions {
    pub display: Display,
    /// Overrides on top of the display profile.
    pub threshold: Option<u8>,
    pub min_size: Option<u32>,
    pub flip: Option<Flip>,
    pub orientation: Option<f64>,
    pub cal_x: Option<f64>,
    pub cal_y: Option<f64>,
    /// Requested mmap buffer count.
    pub buffers: u32,
    /// Stop after this many frames; 0 = until `stop` is set.
    pub frames: u32,
    /// Save frames and a CSV here.
    pub record: Option<PathBuf>,
    /// Print one line per frame (diagnostics).
    pub per_frame: bool,
    /// Report a status line every `report_every` (None = never).
    pub report_every: Option<Duration>,
}

/// One processed frame, handed to the caller's hook.
pub struct Sample<'a> {
    /// The encoded frame exactly as the camera delivered it, for saving alongside a decision.
    pub raw: &'a [u8],
    pub aim: Option<[f64; 2]>,
    pub quad: Option<Quad>,
    pub sequence: u32,
    pub age: Option<Duration>,
    pub events: &'a [crate::protocol::event::Event],
}

/// What the hook wants the loop to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// A hook that does nothing, for callers that just want the loop to run.
pub fn no_hook(_: &Sample) -> Flow {
    Flow::Continue
}

/// Live status of one tracker, for whoever is watching.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub name: String,
    pub frames: u64,
    pub found: u64,
    pub fps: f64,
    pub last_aim: Option<[f64; 2]>,
    pub last_quad: Option<[[f64; 2]; 4]>,
    pub clipped: bool,
    pub proc_mean: Duration,
    pub age_mean: Duration,
    pub events: u64,
    pub error: Option<String>,
    /// The camera pixel treated as the bore axis, after any flip.
    pub aim_pixel: [f64; 2],
    /// Camera frame size in pixels.
    pub frame: (usize, usize),
}

pub fn apply_display_to_camera(dev: &Device, d: &Display) -> Result<()> {
    if let Some(fps) = d.fps {
        let (n, den) = dev.set_frame_interval(1, fps)?;
        info!("frame interval {n}/{den} s");
    }
    match d.exposure {
        Exposure::Manual(v) => dev.set_manual_exposure(v)?,
        Exposure::Auto(_) => dev.set_auto_exposure()?,
    }
    for (id, val, name) in [
        (cid::BRIGHTNESS, d.brightness, "brightness"),
        (cid::CONTRAST, d.contrast, "contrast"),
        (cid::GAIN, d.gain, "gain"),
        (cid::GAMMA, d.gamma, "gamma"),
        (cid::SHARPNESS, d.sharpness, "sharpness"),
    ] {
        if let Some(v) = val {
            dev.set_control(id, v)
                .with_context(|| format!("setting {name}={v}"))?;
        }
    }
    Ok(())
}

/// Run one tracker until `frames` is reached or `stop` is set. `gun` is `Some` to drive a gun
/// (it must already be authenticated, configured and streaming); `None` only tracks.
#[allow(clippy::too_many_lines)]
pub fn run_tracker(
    name: &str,
    camera: &std::path::Path,
    gun: Option<(Gun, GunConfig)>,
    opts: &TrackerOptions,
    stop: &AtomicBool,
    status: &Arc<Mutex<Status>>,
) -> Result<()> {
    run_tracker_with(name, camera, gun, opts, stop, status, &mut no_hook)
}

/// As [`run_tracker`], but calling `hook` once per processed frame so the caller can run its
/// own state machine (the aim test does this) and stop the loop when it is finished.
#[allow(clippy::too_many_arguments)]
pub fn run_tracker_with(
    name: &str,
    camera: &std::path::Path,
    mut gun: Option<(Gun, GunConfig)>,
    opts: &TrackerOptions,
    stop: &AtomicBool,
    status: &Arc<Mutex<Status>>,
    hook: &mut dyn FnMut(&Sample) -> Flow,
) -> Result<()> {
    let dev = Device::open(camera).with_context(|| format!("opening {}", camera.display()))?;
    let fmt = dev.set_format(640, 480, PixelFormat::Mjpeg)?;
    apply_display_to_camera(&dev, &opts.display)?;
    let (w, h) = (fmt.width as usize, fmt.height as usize);
    let d = &opts.display;
    let threshold = opts.threshold.unwrap_or(d.threshold);
    let min_size = opts.min_size.unwrap_or(d.min_size);
    let orientation = opts.orientation.unwrap_or(d.orientation);
    let flip = opts.flip.unwrap_or(match d.flip {
        crate::config::Flip::None => Flip::None,
        crate::config::Flip::Horizontal => Flip::Horizontal,
        crate::config::Flip::Vertical => Flip::Vertical,
        crate::config::Flip::Both => Flip::Both,
    });
    let (mut cal_x, mut cal_y) = (opts.cal_x, opts.cal_y);
    if let Some((g, gc)) = gun.as_mut() {
        cal_x = cal_x.or(gc.calibration_x);
        cal_y = cal_y.or(gc.calibration_y);
        if cal_x.is_none() {
            cal_x = Some(g.calibration_x()?.0);
        }
        if cal_y.is_none() {
            cal_y = Some(g.calibration_y()?.0);
        }
    }
    let (cal_x, cal_y) = (cal_x.unwrap_or(0.0), cal_y.unwrap_or(0.0));
    // The bore offset lives in the camera's frame, so it must go through the same flip as the
    // image; otherwise it is applied backwards and every aim point carries twice the offset.
    let aim_px = flip_point(aim_pixel(w, h, cal_x, cal_y, orientation), w, h, flip);
    let params = AcquireParams {
        threshold,
        min_size,
        ..Default::default()
    };
    info!(
        "[{name}] tracking {}x{} {} on {}, aim pixel ({:.1}, {:.1}), threshold {threshold}, flip {flip:?}, bore ({cal_x:+.2}%, {cal_y:+.2}%)",
        w, h, fmt.format, camera.display(), aim_px[0], aim_px[1]
    );

    let mut csv = None;
    if let Some(dir) = &opts.record {
        std::fs::create_dir_all(dir)?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("frames.csv"))?);
        writeln!(
            f,
            "seq,ts_us,age_us,proc_us,found,clipped,tlx,tly,trx,try,brx,bry,blx,bly,aimx,aimy"
        )?;
        csv = Some(f);
    }

    let mut stream = dev.start_stream(opts.buffers)?;
    let start = Instant::now();
    let mut n = 0u32;
    let mut found = 0u64;
    let mut proc_sum = Duration::ZERO;
    let mut age_sum = Duration::ZERO;
    let mut age_n = 0u32;
    let mut last_report = Instant::now();
    let mut last_aim: Option<[f64; 2]> = None;
    let mut last_quad: Option<Quad> = None;
    let mut events = 0u64;
    let mut window_start = Instant::now();
    let mut window_frames = 0u32;
    let mut fps = 0.0;
    while !stop.load(Ordering::Relaxed) && (opts.frames == 0 || n < opts.frames) {
        let Some(frame) = stream.next(Some(Duration::from_secs(3)), true)? else {
            warn!("[{name}] frame timeout");
            continue;
        };
        n += 1;
        window_frames += 1;
        if window_start.elapsed() >= Duration::from_secs(1) {
            fps = f64::from(window_frames) / window_start.elapsed().as_secs_f64();
            window_start = Instant::now();
            window_frames = 0;
        }
        let t0 = Instant::now();
        let Ok((_, _, mut l)) = luma::mjpeg_to_luma(frame.data) else {
            warn!("[{name}] jpeg decode failed");
            continue;
        };
        if l.len() != w * h {
            continue;
        }
        flip_luma(&mut l, w, flip);
        let quad = acquire(&l, w, h, &params);
        let mut aim = None;
        if let Some(q) = &quad {
            found += 1;
            if let Some(p) = q
                .to_screen()
                .apply(aim_px)
                .filter(|p| (-25.0..=125.0).contains(&p[0]) && (-25.0..=125.0).contains(&p[1]))
            {
                let (x, y) = d.finish_aim(p[0], p[1]);
                aim = Some([x, y]);
                if let Some((g, _)) = gun.as_mut() {
                    g.set_position(percent_to_axis(x), percent_to_axis(y))?;
                }
            }
            last_quad = Some(*q);
        } else if let (Some((g, _)), Some(prev)) = (gun.as_mut(), last_aim) {
            g.set_position(percent_to_axis(prev[0]), percent_to_axis(prev[1]))?;
        }
        if aim.is_some() {
            last_aim = aim;
        }
        let proc = t0.elapsed();
        proc_sum += proc;
        if let Some(age) = frame.age() {
            age_sum += age;
            age_n += 1;
        }
        let mut frame_events = Vec::new();
        if let Some((g, _)) = gun.as_mut() {
            frame_events = g.poll_events()?;
            events += frame_events.len() as u64;
        }
        if let Some(f) = csv.as_mut() {
            let (c, clipped) = quad.map_or(([[f64::NAN; 2]; 4], false), |q| (q.corners, q.clipped));
            let am = aim.unwrap_or([f64::NAN; 2]);
            writeln!(
                f,
                "{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.2},{:.2}",
                frame.sequence,
                frame.timestamp.map_or(0, |t| t.as_micros()),
                frame.age().map_or(0, |t| t.as_micros()),
                proc.as_micros(),
                u8::from(quad.is_some()),
                u8::from(clipped),
                c[0][0],
                c[0][1],
                c[1][0],
                c[1][1],
                c[2][0],
                c[2][1],
                c[3][0],
                c[3][1],
                am[0],
                am[1]
            )?;
            if let Some(dir) = &opts.record {
                std::fs::write(
                    dir.join(format!("frame_{:06}.jpg", frame.sequence)),
                    frame.data,
                )?;
            }
        }
        let report = opts.per_frame
            || opts
                .report_every
                .is_some_and(|e| last_report.elapsed() >= e);
        if report {
            last_report = Instant::now();
            match (&quad, aim) {
                (Some(q), Some(am)) => println!(
                    "[{name}] {:>7.2}s seq={:<6} aim=({:6.2}%, {:6.2}%) TL({:.0},{:.0}) TR({:.0},{:.0}) BR({:.0},{:.0}) BL({:.0},{:.0}){} proc={:.2}ms age={:.1}ms",
                    start.elapsed().as_secs_f64(), frame.sequence, am[0], am[1],
                    q.corners[0][0], q.corners[0][1], q.corners[1][0], q.corners[1][1],
                    q.corners[2][0], q.corners[2][1], q.corners[3][0], q.corners[3][1],
                    if q.clipped { " CLIPPED" } else { "" },
                    proc.as_secs_f64() * 1000.0, frame.age().map_or(0.0, |a| a.as_secs_f64() * 1000.0)
                ),
                _ => {
                    let (mean, max) = luma::luma_stats(&l);
                    println!("[{name}] {:>7.2}s seq={:<6} no border (luma mean {:.1} max {}) proc={:.2}ms", start.elapsed().as_secs_f64(), frame.sequence, mean, max, proc.as_secs_f64() * 1000.0);
                }
            }
        }
        let flow = hook(&Sample {
            raw: frame.data,
            aim,
            quad,
            sequence: frame.sequence,
            age: frame.age(),
            events: &frame_events,
        });
        if let Ok(mut s) = status.lock() {
            s.aim_pixel = aim_px;
            s.frame = (w, h);
            s.name = name.to_owned();
            s.frames = u64::from(n);
            s.found = found;
            s.fps = fps;
            s.last_aim = last_aim;
            s.last_quad = last_quad.map(|q| q.corners);
            s.clipped = last_quad.is_some_and(|q| q.clipped);
            s.proc_mean = proc_sum / n.max(1);
            s.age_mean = if age_n > 0 {
                age_sum / age_n
            } else {
                Duration::ZERO
            };
            s.events = events;
        }
        if flow == Flow::Stop {
            break;
        }
    }
    info!(
        "[{name}] done: frames={} found={} ({:.0}%) proc mean {:.2}ms",
        n,
        found,
        if n > 0 {
            found as f64 * 100.0 / f64::from(n)
        } else {
            0.0
        },
        (proc_sum / n.max(1)).as_secs_f64() * 1000.0
    );
    Ok(())
}

/// Bring a gun up for tracking: authenticate is the caller's job (recovery lives in the CLI);
/// this applies config and starts streaming.
pub fn prepare_gun(gun: &mut Gun, cfg: &GunConfig, recoil_gap: Duration) -> Result<()> {
    gun.apply_config(cfg, recoil_gap)?;
    gun.start_streaming(Duration::from_millis(150))?;
    Ok(())
}
