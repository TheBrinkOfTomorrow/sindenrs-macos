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
use crate::vision::lens::Lens;
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
    /// Camera lens distortion coefficient (config: global.lens_k1).
    pub lens_k1: f64,
    /// Aim jump, in screen percent, above which a frame whose solve rests on less than the
    /// previous one is held for a frame (config: display.jump_limit).
    pub jump_limit: f64,
    /// Aim tracker blend weight for a four-edge solve (config: display.hover_smoothing;
    /// 1 = off); weaker solves are smoothed harder.
    pub hover_smoothing: f64,
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
    /// The camera frame's corners in screen percent (TL, TR, BR, BL of the image), i.e.
    /// where on the screen the camera is looking, when a quad was solved.
    pub view: Option<[[f64; 2]; 4]>,
    pub sequence: u32,
    pub age: Option<Duration>,
    pub events: &'a [crate::protocol::event::Event],
}

/// How much a solve rests on: sides with an edge line, and decoded tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Support {
    pub sides: u32,
    pub tabs: u8,
}

/// Holds back a single frame whose aim leaps away from the last one while its solve rests
/// on less than the last one did. A change of regime (four edges to three, three to two
/// plus tabs) is where a wrong solve slips through, and it shows as exactly that: a big
/// jump on a weaker frame. Real motion is let through unchanged, and a jump that the next
/// frame confirms is accepted then, so the cost is one frame of delay on a fast flick that
/// coincides with a regime change.
#[derive(Clone, Copy, Debug)]
pub struct JumpGuard {
    /// Jump size in screen percent above which a weaker frame is held.
    pub limit: f64,
    last: Option<([f64; 2], Support)>,
    pending: Option<[f64; 2]>,
}

impl JumpGuard {
    #[must_use]
    pub fn new(limit: f64) -> Self {
        Self {
            limit,
            last: None,
            pending: None,
        }
    }

    /// Feed a frame's aim and support; get back the aim to act on, or `None` to hold.
    pub fn filter(&mut self, aim: [f64; 2], support: Support) -> Option<[f64; 2]> {
        let dist = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
        let suspicious = self.last.is_some_and(|(prev, ps)| {
            let weaker = support.sides < ps.sides
                || (support.sides == ps.sides && support.sides < 4 && support.tabs + 2 < ps.tabs);
            weaker && dist(aim, prev) > self.limit
        });
        if suspicious {
            match self.pending {
                Some(p) if dist(aim, p) <= self.limit => {
                    // Two frames in a row agree: it was real.
                    self.pending = None;
                    self.last = Some((aim, support));
                    Some(aim)
                }
                _ => {
                    self.pending = Some(aim);
                    None
                }
            }
        } else {
            self.pending = None;
            self.last = Some((aim, support));
            Some(aim)
        }
    }

    /// No aim this frame: forget any pending jump.
    pub fn lost(&mut self) {
        self.pending = None;
    }
}

/// Smooths the aim with a constant-velocity (alpha-beta) tracker: each frame predicts
/// the aim from the last position and velocity and blends in the measurement, so steady
/// motion is followed without lag while frame-to-frame solve noise is averaged down. The
/// blend weight follows the solve's support: four edges are precise and get followed
/// closely, two edges are noisier and get smoothed harder. A residual beyond `reset` is
/// taken as real (a flick, or the guard letting a confirmed jump through) and the tracker
/// restarts on the measurement.
#[derive(Clone, Copy, Debug)]
pub struct HoverSmoother {
    /// Residual, in screen percent, beyond which the tracker restarts on the measurement.
    pub reset: f64,
    /// Position blend weight for a four-edge solve (1 = no smoothing); weaker solves use
    /// a fraction of it.
    pub factor: f64,
    pos: Option<[f64; 2]>,
    vel: [f64; 2],
}

impl HoverSmoother {
    #[must_use]
    pub fn new(reset: f64, factor: f64) -> Self {
        Self {
            reset,
            factor: factor.clamp(0.05, 1.0),
            pos: None,
            vel: [0.0, 0.0],
        }
    }

    pub fn filter(&mut self, aim: [f64; 2], support: Support) -> [f64; 2] {
        let Some(p) = self.pos else {
            self.pos = Some(aim);
            return aim;
        };
        let pred = [p[0] + self.vel[0], p[1] + self.vel[1]];
        let r = [aim[0] - pred[0], aim[1] - pred[1]];
        if r[0].hypot(r[1]) > self.reset {
            self.pos = Some(aim);
            self.vel = [0.0, 0.0];
            return aim;
        }
        let alpha = self.factor
            * match support.sides {
                4 => 1.0,
                3 => 0.6,
                _ => 0.4,
            };
        let beta = alpha * alpha / 4.0;
        let out = [pred[0] + alpha * r[0], pred[1] + alpha * r[1]];
        self.vel = [self.vel[0] + beta * r[0], self.vel[1] + beta * r[1]];
        self.pos = Some(out);
        out
    }

    /// Tracking was lost: start afresh on the next aim.
    pub fn reset(&mut self) {
        self.pos = None;
        self.vel = [0.0, 0.0];
    }
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
    /// Frames whose aim the jump guard held back.
    pub held: u64,
    /// Frames the camera link delivered truncated or undecodable.
    pub corrupt: u64,
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
    let busy = |e: anyhow::Error| {
        if e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.raw_os_error() == Some(16))
        {
            e.context(format!(
                "{} is busy: another process is streaming it (a `track`, `run` or \
                 `aim-test` still going?); only one can use a camera at a time",
                camera.display()
            ))
        } else {
            e
        }
    };
    let fmt = dev
        .set_format(640, 480, PixelFormat::Mjpeg)
        .map_err(|e| busy(e.into()))?;
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
        lens_k1: opts.lens_k1,
        screen_aspect: d.aspect,
        border_frac: d.border_thickness / 100.0,
        ..Default::default()
    };
    // Corners come out in undistorted pixels, so the aim pixel has to be undistorted too.
    let lens = Lens::centred(opts.lens_k1, w, h);
    let aim_undist = lens.undistort(aim_px);
    #[allow(clippy::cast_precision_loss)]
    let frame_corners = [
        [0.0, 0.0],
        [(w - 1) as f64, 0.0],
        [(w - 1) as f64, (h - 1) as f64],
        [0.0, (h - 1) as f64],
    ]
    .map(|c| lens.undistort(c));
    info!(
        "[{name}] tracking {}x{} {} on {}, aim pixel ({:.1}, {:.1}), threshold {threshold}, flip {flip:?}, bore ({cal_x:+.2}%, {cal_y:+.2}%), lens k1 {}",
        w, h, fmt.format, camera.display(), aim_px[0], aim_px[1], opts.lens_k1
    );

    let mut csv = None;
    if let Some(dir) = &opts.record {
        std::fs::create_dir_all(dir)?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("frames.csv"))?);
        writeln!(
            f,
            "seq,ts_us,age_us,proc_us,found,clipped,tabs,tlx,tly,trx,try,brx,bry,blx,bly,aimx,aimy"
        )?;
        csv = Some(f);
    }

    let mut stream = dev.start_stream(opts.buffers).map_err(|e| busy(e.into()))?;
    let start = Instant::now();
    let mut n = 0u32;
    let mut found = 0u64;
    let mut proc_sum = Duration::ZERO;
    let mut age_sum = Duration::ZERO;
    let mut age_n = 0u32;
    let mut last_report = Instant::now();
    let mut last_aim: Option<[f64; 2]> = None;
    let mut last_quad: Option<Quad> = None;
    let mut guard = JumpGuard::new(opts.jump_limit);
    let mut sizes: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut corrupt = 0u64;
    let mut corrupt_window = 0u64;
    let mut last_corrupt_report = Instant::now();
    let mut smoother = HoverSmoother::new(4.0, opts.hover_smoothing);
    let mut held = 0u64;
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
        // A frame the USB link truncated decodes (non-strict) with its missing part as flat
        // grey, which the solver would then read as a bright screen. A truncated frame is
        // far smaller than its neighbours, so compare against the recent median size.
        // Corrupt frames are counted and reported once a second: a stream of them means
        // the camera's link (hub, cable, EMI from the recoil) is failing, and a warning
        // per frame at 60 fps would bury everything else.
        let size = frame.data.len();
        let median_size = {
            let mut v: Vec<usize> = sizes.iter().copied().collect();
            v.sort_unstable();
            v.get(v.len() / 2).copied()
        };
        let truncated = median_size.is_some_and(|m| sizes.len() >= 10 && size * 10 < m * 6);
        if !truncated {
            sizes.push_back(size);
            if sizes.len() > 30 {
                sizes.pop_front();
            }
        }
        let decoded = if truncated {
            None
        } else {
            luma::mjpeg_to_luma(frame.data).ok()
        };
        let Some((_, _, mut l)) = decoded else {
            corrupt += 1;
            corrupt_window += 1;
            if last_corrupt_report.elapsed() >= Duration::from_secs(1) {
                warn!(
                    "[{name}] {corrupt_window} corrupt frames in the last second ({} total): the camera's USB link is dropping data (hub, cable, or EMI from the recoil?)",
                    corrupt
                );
                last_corrupt_report = Instant::now();
                corrupt_window = 0;
            }
            continue;
        };
        if l.len() != w * h {
            continue;
        }
        flip_luma(&mut l, w, flip);
        let quad = acquire(&l, w, h, &params);
        let mut aim = None;
        let mut view = None;
        if let Some(q) = &quad {
            found += 1;
            let m = q.to_screen();
            let vs = frame_corners.map(|c| m.apply(c));
            if vs.iter().all(Option::is_some) {
                view = Some(vs.map(|v| v.unwrap_or([0.0, 0.0])));
            }
            // A hull quad is for showing roughly where the border is; its corners are
            // synthesised and aiming from them threw the cursor about at the corners of
            // the screen, where hull and tab solves alternate frame by frame.
            let candidate = Some(q)
                .filter(|q| q.from_lines)
                .and_then(|q| q.to_screen().apply(aim_undist))
                .filter(|p| (-25.0..=125.0).contains(&p[0]) && (-25.0..=125.0).contains(&p[1]))
                .map(|p| {
                    let (x, y) = d.finish_aim(p[0], p[1]);
                    [x, y]
                });
            let support = Support {
                sides: q.sides.count_ones(),
                tabs: q.tabs,
            };
            aim = candidate
                .and_then(|a| guard.filter(a, support))
                .map(|a| smoother.filter(a, support));
            if candidate.is_some() && aim.is_none() {
                held += 1;
            }
            if candidate.is_none() {
                guard.lost();
                smoother.reset();
            }
            if let Some((g, _)) = gun.as_mut() {
                if let Some([x, y]) = aim.or(last_aim) {
                    g.set_position(percent_to_axis(x), percent_to_axis(y))?;
                }
            }
            last_quad = Some(*q);
        } else {
            guard.lost();
            smoother.reset();
            if let (Some((g, _)), Some(prev)) = (gun.as_mut(), last_aim) {
                g.set_position(percent_to_axis(prev[0]), percent_to_axis(prev[1]))?;
            }
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
            let (c, clipped, tabs) = quad.map_or(([[f64::NAN; 2]; 4], false, 0), |q| {
                (q.corners, q.clipped, q.tabs)
            });
            let am = aim.unwrap_or([f64::NAN; 2]);
            writeln!(
                f,
                "{},{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.2},{:.2}",
                frame.sequence,
                frame.timestamp.map_or(0, |t| t.as_micros()),
                frame.age().map_or(0, |t| t.as_micros()),
                proc.as_micros(),
                u8::from(quad.is_some()),
                u8::from(clipped),
                tabs,
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
                    match (q.from_lines, q.tabs) {
                        (false, _) => " CLIPPED",
                        (true, 0) => "",
                        (true, _) => " coded",
                    },
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
            view,
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
            s.held = held;
            s.corrupt = corrupt;
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

#[cfg(test)]
mod tests {
    use super::*;

    const STRONG: Support = Support { sides: 4, tabs: 20 };
    const WEAK: Support = Support { sides: 3, tabs: 6 };

    #[test]
    fn guard_passes_motion_and_holds_a_weak_leap() {
        let mut g = JumpGuard::new(5.0);
        assert_eq!(g.filter([10.0, 10.0], STRONG), Some([10.0, 10.0]));
        // Steady motion on a strong solve is never held, however far it goes.
        assert_eq!(g.filter([30.0, 10.0], STRONG), Some([30.0, 10.0]));
        // A weaker solve leaping away is held once...
        assert_eq!(g.filter([80.0, 60.0], WEAK), None);
        // ...and accepted when the next frame agrees with it.
        assert_eq!(g.filter([81.0, 61.0], WEAK), Some([81.0, 61.0]));
    }

    #[test]
    fn guard_drops_a_one_frame_glitch() {
        let mut g = JumpGuard::new(5.0);
        g.filter([10.0, 10.0], STRONG);
        assert_eq!(g.filter([80.0, 60.0], WEAK), None);
        // Back near where we were: the glitch is gone, nothing was sent for it.
        assert_eq!(g.filter([11.0, 10.0], STRONG), Some([11.0, 10.0]));
    }

    #[test]
    fn smoother_damps_jitter_and_follows_motion_without_lag() {
        let mut s = HoverSmoother::new(4.0, 0.5);
        assert_eq!(s.filter([10.0, 10.0], STRONG), [10.0, 10.0]);
        // A 0.4% wobble is halved.
        let o = s.filter([10.4, 10.0], STRONG);
        assert!((o[0] - 10.2).abs() < 1e-9, "{o:?}");
        // Steady motion of 1% per frame: after a few frames the tracker rides along with
        // little lag, and the lag keeps shrinking.
        let mut s = HoverSmoother::new(4.0, 0.5);
        let mut lag = Vec::new();
        for k in 0..40 {
            let x = 10.0 + f64::from(k);
            let o = s.filter([x, 20.0], STRONG);
            lag.push(x - o[0]);
        }
        assert!(lag[39].abs() < 0.2, "lag {lag:?}");
        assert!(lag[39].abs() < lag[5].abs());
        // A flick restarts on the measurement.
        assert_eq!(s.filter([80.0, 60.0], STRONG), [80.0, 60.0]);
    }

    #[test]
    fn guard_lets_small_regime_shifts_through() {
        let mut g = JumpGuard::new(5.0);
        g.filter([10.0, 10.0], STRONG);
        assert_eq!(g.filter([12.0, 11.0], WEAK), Some([12.0, 11.0]));
    }
}
