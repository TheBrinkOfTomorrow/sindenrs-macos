//! A live view of the tracker, for tuning and for testing aim: the camera feed, what the
//! detector made of it, and shots against targets on a full-screen page that also carries
//! the border. The state and the pixel work are platform-neutral and live here; the window
//! is per platform (`appkit` on macOS).
//!
//! Everything drawn inside the border is kept dim on purpose: the camera is looking at this
//! page, and a bright copy of the border in a preview panel would compete with the real one.

#[cfg(target_os = "macos")]
pub mod appkit;
#[cfg(target_os = "macos")]
pub mod hud;

use std::collections::{BTreeMap, VecDeque};

use crate::overlay::draw::{draw_border, px, Canvas, BLACK};
use crate::runtime::Sample;

/// Scale for everything drawn inside the border, so nothing there reads as border to the
/// camera (the detector's threshold is around 48 of 255 at the tracking exposure).
pub const DIM: f64 = 0.25;

/// Where the shooting targets sit, in screen percent.
pub const TARGETS: [[f64; 2]; 5] = [
    [50.0, 50.0],
    [20.0, 20.0],
    [80.0, 20.0],
    [20.0, 80.0],
    [80.0, 80.0],
];

/// How many log lines the window keeps.
const LOG_LINES: usize = 12;

/// An RGB image, row-major `0x00RRGGBB`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
}

/// Written by the tracker thread, read by the window.
#[derive(Debug, Default)]
pub struct Shared {
    /// The camera feed as the solve saw it (after the flip), dimmed.
    pub camera: Image,
    /// Pixels over the threshold, the detected quad and the aim pixel.
    pub processed: Image,
    /// Bumped whenever the images change.
    pub serial: u64,
    /// One line of tracker status.
    pub status: String,
    /// The tracker's latest aim in screen percent.
    pub aim: Option<[f64; 2]>,
    /// Shots, clicks, keys and gun events, newest first (what the window shows).
    pub log: VecDeque<String>,
    /// Every log line, oldest first, for printing when the preview closes.
    pub history: Vec<String>,
    /// The tracker has finished; the window should close.
    pub done: bool,
}

impl Shared {
    pub fn push_log(&mut self, line: String) {
        self.history.push(line.clone());
        self.log.push_front(line);
        self.log.truncate(LOG_LINES);
    }
}

/// What `run`'s heads-up display shows (macOS: `hud`): per gun, the aim for its reticle and,
/// while the camera view is on, its preview images. Written by the trackers, read by the window.
#[derive(Debug, Default)]
pub struct Live {
    /// Show a reticle at each gun's aim.
    pub reticle: bool,
    /// Show each gun's camera view (the feed and what the detector made of it).
    pub camera: bool,
    /// Keyed by gun name, so the order on screen is stable.
    pub guns: BTreeMap<String, LiveGun>,
}

#[derive(Debug, Default)]
pub struct LiveGun {
    pub aim: Option<[f64; 2]>,
    pub camera: Image,
    pub processed: Image,
    /// Bumped whenever the images change.
    pub serial: u64,
}

/// Frames between preview images: about 15 per second at 60 fps, plenty to watch and cheap.
const PREVIEW_EVERY: u64 = 4;

impl Live {
    /// A tracker's hook: record gun `name`'s aim, and its images if the camera view is on.
    /// `frame` counts the tracker's frames. Images are made outside the lock.
    pub fn feed(live: &std::sync::Mutex<Self>, name: &str, s: &Sample, frame: u64) {
        let want_images =
            frame.is_multiple_of(PREVIEW_EVERY) && live.lock().is_ok_and(|l| l.camera);
        let images = want_images.then(|| images_for(s));
        if let Ok(mut l) = live.lock() {
            let g = l.guns.entry(name.to_owned()).or_default();
            g.aim = s.aim;
            if let Some((camera, processed)) = images {
                g.camera = camera;
                g.processed = processed;
                g.serial += 1;
            }
        }
    }

    /// A gun's tracker ended: drop its reticle and panels.
    pub fn remove(live: &std::sync::Mutex<Self>, name: &str) {
        if let Ok(mut l) = live.lock() {
            l.guns.remove(name);
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dim(v: u8, k: f64) -> u32 {
    (f64::from(v) * k).round().clamp(0.0, 255.0) as u32
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r << 16) | (g << 8) | b
}

/// The camera feed as a grey image, scaled by `k`.
pub fn camera_image(luma: &[u8], w: usize, h: usize, k: f64) -> Image {
    let px = luma
        .iter()
        .take(w * h)
        .map(|&v| {
            let g = dim(v, k);
            rgb(g, g, g)
        })
        .collect();
    Image { w, h, px }
}

/// What the detector saw: pixels at or over `threshold` in amber, the solved quad (green
/// when solved from edge lines, red when only from the hull), and the aim pixel in cyan.
/// Colours are scaled by `k`. The quad is drawn in undistorted pixels on the distorted image,
/// so with a strong lens it sits slightly off the edges it came from.
pub fn processed_image(
    luma: &[u8],
    w: usize,
    h: usize,
    threshold: u8,
    quad: Option<([[f64; 2]; 4], bool)>,
    aim_pixel: [f64; 2],
    k: f64,
) -> Image {
    let amber = rgb(dim(255, k), dim(170, k), dim(40, k));
    let mut buf: Vec<u32> = luma
        .iter()
        .take(w * h)
        .map(|&v| if v >= threshold { amber } else { BLACK })
        .collect();
    buf.resize(w * h, BLACK);
    let (cw, ch) = (i64::try_from(w).unwrap_or(0), i64::try_from(h).unwrap_or(0));
    let mut c = Canvas {
        px: &mut buf,
        w: cw,
        h: ch,
    };
    if let Some((corners, from_lines)) = quad {
        let colour = if from_lines {
            rgb(dim(60, k), dim(255, k), dim(60, k))
        } else {
            rgb(dim(255, k), dim(50, k), dim(50, k))
        };
        for i in 0..4 {
            let (a, b) = (corners[i], corners[(i + 1) % 4]);
            c.line(px(a[0]), px(a[1]), px(b[0]), px(b[1]), 2, colour);
        }
    }
    let cyan = rgb(dim(60, k), dim(230, k), dim(230, k));
    c.cross(px(aim_pixel[0]), px(aim_pixel[1]), 10, 2, cyan);
    Image { w, h, px: buf }
}

/// Both preview images for one tracker sample.
pub fn images_for(s: &Sample) -> (Image, Image) {
    let quad = s.quad.map(|q| (q.corners, q.from_lines));
    (
        camera_image(s.luma, s.width, s.height, DIM),
        processed_image(
            s.luma,
            s.width,
            s.height,
            s.threshold,
            quad,
            s.aim_pixel,
            DIM,
        ),
    )
}

/// The page itself at `w`x`h`: black, the coded border, and the targets (dim red rings with
/// a cross) for aiming at.
pub fn page(w: u32, h: u32, border_frac: f64) -> Image {
    let (wu, hu) = (w as usize, h as usize);
    let mut buf = vec![BLACK; wu * hu];
    let mut c = Canvas {
        px: &mut buf,
        w: i64::from(w),
        h: i64::from(h),
    };
    draw_border(&mut c, w, h, border_frac);
    let red = rgb(dim(255, 0.38), dim(60, 0.38), dim(60, 0.38));
    let r = px(f64::from(w.min(h)) * 0.03);
    for [tx, ty] in TARGETS {
        let (x, y) = (px(f64::from(w) * tx / 100.0), px(f64::from(h) * ty / 100.0));
        c.ring(x, y, r, 3, red);
        c.cross(x, y, r / 2, 2, red);
    }
    Image {
        w: wu,
        h: hu,
        px: buf,
    }
}

/// The aim marker: a thick ring with a cross, `size` pixels square, on a transparent (zero)
/// background. Red, and kept dark enough to stay under the detection threshold like
/// everything else inside the border, but big enough to see across a room.
pub fn aim_marker(size: usize) -> Image {
    let mut buf = vec![BLACK; size * size];
    let s = i64::try_from(size).unwrap_or(0);
    let mut c = Canvas {
        px: &mut buf,
        w: s,
        h: s,
    };
    let colour = rgb(dim(255, 0.45), dim(40, 0.45), dim(40, 0.45));
    let (mid, r) = (s / 2, s / 2 - s / 16);
    c.ring(mid, mid, r, (s / 12).max(2), colour);
    c.cross(mid, mid, r / 2, (s / 24).max(2), colour);
    Image {
        w: size,
        h: size,
        px: buf,
    }
}

/// The target nearest to a shot at `at` (screen percent), and the shot's error from it.
pub fn score(at: [f64; 2]) -> (usize, [f64; 2]) {
    let err = |t: [f64; 2]| [at[0] - t[0], at[1] - t[1]];
    let dist = |e: [f64; 2]| e[0].hypot(e[1]);
    let mut best = (0, err(TARGETS[0]));
    for (i, &t) in TARGETS.iter().enumerate().skip(1) {
        let e = err(t);
        if dist(e) < dist(best.1) {
            best = (i, e);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shots_score_against_the_nearest_target() {
        assert_eq!(score([50.0, 50.0]), (0, [0.0, 0.0]));
        let (i, e) = score([22.0, 17.5]);
        assert_eq!(i, 1);
        assert!((e[0] - 2.0).abs() < 1e-9 && (e[1] + 2.5).abs() < 1e-9);
        assert_eq!(score([95.0, 95.0]).0, 4);
    }

    #[test]
    fn previews_stay_under_the_detection_threshold() {
        let luma = vec![255u8; 16];
        let cam = camera_image(&luma, 4, 4, DIM);
        let g = cam.px[0] & 0xff;
        assert!(g < 70, "camera preview too bright: {g}");
        let p = processed_image(&luma, 4, 4, 48, None, [100.0, 100.0], DIM);
        let max =
            p.px.iter()
                .flat_map(|v| [v >> 16, (v >> 8) & 0xff, v & 0xff])
                .max()
                .unwrap_or(0);
        assert!(max < 70, "processed preview too bright: {max}");
    }

    #[test]
    fn processed_marks_pixels_over_threshold() {
        let luma = [0u8, 47, 48, 200];
        let p = processed_image(&luma, 4, 1, 48, None, [-50.0, -50.0], 1.0);
        assert_eq!(p.px[0], BLACK);
        assert_eq!(p.px[1], BLACK);
        assert_ne!(p.px[2], BLACK);
        assert_eq!(p.px[2], p.px[3]);
    }

    #[test]
    fn page_has_border_and_dim_targets() {
        let img = page(320, 180, 0.03);
        assert_eq!(img.px[0], crate::overlay::draw::WHITE);
        let centre = img.px[90 * 320 + 160];
        assert_ne!(centre, BLACK, "target cross at the centre");
        assert!((centre >> 16) < 120, "target too bright: {centre:#08x}");
    }

    #[test]
    fn aim_marker_is_a_dim_ring_on_transparent() {
        let m = aim_marker(64);
        assert_eq!(m.px[0], BLACK, "corners stay transparent");
        assert!(m.px[32 * 64 + 32] >> 16 > 0, "cross at the centre");
        let luma = |p: u32| {
            let (r, g, b) = (
                f64::from(p >> 16),
                f64::from((p >> 8) & 0xff),
                f64::from(p & 0xff),
            );
            0.299 * r + 0.587 * g + 0.114 * b
        };
        let max = m.px.iter().map(|&p| luma(p)).fold(0.0, f64::max);
        assert!(max < 70.0, "marker too bright for the camera: luma {max}");
    }

    fn sample<'a>(luma: &'a [u8], aim: Option<[f64; 2]>) -> Sample<'a> {
        Sample {
            raw: &[],
            raw_ext: "pgm",
            luma,
            width: 4,
            height: 4,
            threshold: 48,
            aim_pixel: [2.0, 2.0],
            aim,
            quad: None,
            view: None,
            sequence: 0,
            age: None,
            events: &[],
        }
    }

    #[test]
    fn live_keeps_aim_and_makes_images_only_when_asked() {
        let live = std::sync::Mutex::new(Live::default());
        let luma = [200u8; 16];
        Live::feed(&live, "p1", &sample(&luma, Some([40.0, 60.0])), 4);
        {
            let l = live.lock().expect("lock");
            assert_eq!(l.guns["p1"].aim, Some([40.0, 60.0]));
            assert_eq!(l.guns["p1"].serial, 0, "camera view off: no images");
        }
        live.lock().expect("lock").camera = true;
        Live::feed(&live, "p1", &sample(&luma, None), 5);
        assert_eq!(
            live.lock().expect("lock").guns["p1"].serial,
            0,
            "only every 4th frame"
        );
        Live::feed(&live, "p1", &sample(&luma, None), 8);
        {
            let l = live.lock().expect("lock");
            let g = &l.guns["p1"];
            assert_eq!((g.serial, g.aim), (1, None));
            assert_eq!((g.camera.w, g.camera.h, g.processed.px.len()), (4, 4, 16));
        }
        Live::remove(&live, "p1");
        assert!(live.lock().expect("lock").guns.is_empty());
    }

    #[test]
    fn log_keeps_the_newest_lines() {
        let mut s = Shared::default();
        for i in 0..20 {
            s.push_log(format!("line {i}"));
        }
        assert_eq!(s.log.len(), LOG_LINES);
        assert_eq!(s.log[0], "line 19");
        assert_eq!(s.history.len(), 20);
        assert_eq!(s.history[0], "line 0");
    }
}
