//! Border acquisition: find the bright quadrilateral (the screen border) in a luma frame.
//!
//! This is the "ACQUIRE" mode of the redesign: threshold and 2x2 decimate, label connected
//! components, take each large blob's outer boundary, reduce its convex hull to four
//! corners, then push those corners back to full resolution. It is deliberately the plain,
//! robust version; the sub-pixel edge tracker replaces it in steady state later.

use super::homography::{quad_to_quad, Mat3, P2, SCREEN_PERCENT};

#[derive(Clone, Copy, Debug)]
pub struct AcquireParams {
    /// Luma threshold (0..=255) for "border" at full resolution.
    pub threshold: u8,
    /// Minimum blob width and height, in half-resolution pixels.
    pub min_size: u32,
    /// Corner-refinement search radius at full resolution, in pixels.
    pub refine_radius: i32,
}

impl Default for AcquireParams {
    fn default() -> Self {
        Self {
            threshold: 128,
            min_size: 20,
            refine_radius: 3,
        }
    }
}

/// A binary mask at half resolution.
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub bits: Vec<u8>,
}

/// Threshold at full resolution and decimate 2x2: an output pixel is set if any of its four
/// source pixels is at or above the threshold (what the stock driver does).
pub fn decimate_threshold(luma: &[u8], w: usize, h: usize, threshold: u8) -> Mask {
    let (mw, mh) = (w / 2, h / 2);
    let mut bits = vec![0u8; mw * mh];
    for y in 0..mh {
        let r0 = &luma[(2 * y) * w..(2 * y) * w + w];
        let r1 = &luma[(2 * y + 1) * w..(2 * y + 1) * w + w];
        let out = &mut bits[y * mw..y * mw + mw];
        for x in 0..mw {
            let m = r0[2 * x]
                .max(r0[2 * x + 1])
                .max(r1[2 * x])
                .max(r1[2 * x + 1]);
            out[x] = u8::from(m >= threshold);
        }
    }
    Mask { w: mw, h: mh, bits }
}

#[derive(Clone, Copy, Debug)]
pub struct Blob {
    pub label: u32,
    pub area: u32,
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Blob {
    pub fn width(&self) -> usize {
        self.x1 - self.x0 + 1
    }
    pub fn height(&self) -> usize {
        self.y1 - self.y0 + 1
    }
}

/// 8-connected component labelling. Returns the label map (0 = background) and blobs sorted
/// by bounding-box area, largest first.
pub fn label(mask: &Mask) -> (Vec<u32>, Vec<Blob>) {
    let (w, h) = (mask.w, mask.h);
    let mut labels = vec![0u32; w * h];
    let mut blobs = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 1u32;
    for start in 0..w * h {
        if mask.bits[start] == 0 || labels[start] != 0 {
            continue;
        }
        let id = next;
        next += 1;
        let mut b = Blob {
            label: id,
            area: 0,
            x0: usize::MAX,
            y0: usize::MAX,
            x1: 0,
            y1: 0,
        };
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            b.area += 1;
            b.x0 = b.x0.min(x);
            b.x1 = b.x1.max(x);
            b.y0 = b.y0.min(y);
            b.y1 = b.y1.max(y);
            let ys = y.saturating_sub(1)..=(y + 1).min(h - 1);
            for ny in ys {
                for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                    let j = ny * w + nx;
                    if mask.bits[j] != 0 && labels[j] == 0 {
                        labels[j] = id;
                        stack.push(j);
                    }
                }
            }
        }
        blobs.push(b);
    }
    blobs.sort_by_key(|b| std::cmp::Reverse(b.width() * b.height()));
    (labels, blobs)
}

/// Pixels of `blob` that touch the outside (4-neighbourhood) or the image edge.
pub fn boundary_points(labels: &[u32], w: usize, h: usize, blob: &Blob) -> Vec<P2> {
    let mut out = Vec::new();
    for y in blob.y0..=blob.y1 {
        for x in blob.x0..=blob.x1 {
            if labels[y * w + x] != blob.label {
                continue;
            }
            let edge = x == 0
                || y == 0
                || x == w - 1
                || y == h - 1
                || labels[y * w + x - 1] != blob.label
                || labels[y * w + x + 1] != blob.label
                || labels[(y - 1) * w + x] != blob.label
                || labels[(y + 1) * w + x] != blob.label;
            if edge {
                #[allow(clippy::cast_precision_loss)]
                out.push([x as f64, y as f64]);
            }
        }
    }
    out
}

fn cross(o: P2, a: P2, b: P2) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Convex hull (Andrew's monotone chain), counter-clockwise in image coordinates
/// (y down), without the repeated first point.
pub fn convex_hull(points: &[P2]) -> Vec<P2> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| {
        a[0].partial_cmp(&b[0])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
    });
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let mut lower: Vec<P2> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<P2> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn point_line_dist(p: P2, a: P2, b: P2) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-12 {
        return ((p[0] - a[0]).powi(2) + (p[1] - a[1]).powi(2)).sqrt();
    }
    (cross(a, b, p) / len).abs()
}

/// Ramer–Douglas–Peucker on an open polyline, keeping endpoints.
fn rdp(pts: &[P2], eps: f64, out: &mut Vec<P2>) {
    if pts.len() < 3 {
        out.extend_from_slice(pts);
        return;
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (mut best, mut best_d) = (0, -1.0);
    for (i, &p) in pts.iter().enumerate().skip(1).take(pts.len() - 2) {
        let d = point_line_dist(p, a, b);
        if d > best_d {
            best_d = d;
            best = i;
        }
    }
    if best_d > eps {
        let mut left = Vec::new();
        rdp(&pts[..=best], eps, &mut left);
        left.pop();
        out.extend(left);
        rdp(&pts[best..], eps, out);
    } else {
        out.push(a);
        out.push(b);
    }
}

/// Reduce a convex hull to four corners with RDP at increasing tolerance.
/// Returns `None` if no tolerance yields exactly four vertices with sane geometry.
pub fn approx_quad(hull: &[P2]) -> Option<[P2; 4]> {
    if hull.len() < 4 {
        return None;
    }
    // Start the closed polyline at the vertex farthest from the centroid (a likely corner)
    // so RDP keeps it.
    let n = hull.len();
    #[allow(clippy::cast_precision_loss)]
    let c = hull.iter().fold([0.0, 0.0], |a, p| {
        [a[0] + p[0] / n as f64, a[1] + p[1] / n as f64]
    });
    let start = (0..n)
        .max_by(|&i, &j| {
            let di = (hull[i][0] - c[0]).powi(2) + (hull[i][1] - c[1]).powi(2);
            let dj = (hull[j][0] - c[0]).powi(2) + (hull[j][1] - c[1]).powi(2);
            di.partial_cmp(&dj).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let mut closed: Vec<P2> = (0..=n).map(|k| hull[(start + k) % n]).collect();
    let mut eps = 1.0;
    for _ in 0..12 {
        let mut out = Vec::new();
        rdp(&closed, eps, &mut out);
        out.pop(); // closing point repeats the first
        match out.len() {
            4 => {
                let q: [P2; 4] = [out[0], out[1], out[2], out[3]];
                // Reject degenerate quads (a corner with a nearly straight angle).
                for i in 0..4 {
                    let (a, b, d) = (q[(i + 3) % 4], q[i], q[(i + 1) % 4]);
                    let (v1, v2) = ([a[0] - b[0], a[1] - b[1]], [d[0] - b[0], d[1] - b[1]]);
                    let l1 = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
                    let l2 = (v2[0] * v2[0] + v2[1] * v2[1]).sqrt();
                    if l1 < 1e-9 || l2 < 1e-9 {
                        return None;
                    }
                    let cos = (v1[0] * v2[0] + v1[1] * v2[1]) / (l1 * l2);
                    if cos < -0.94 {
                        return None; // > ~160 degrees: not a real corner
                    }
                }
                return Some(q);
            }
            k if k < 4 => return None,
            _ => {
                eps *= 1.6;
                closed = out;
                closed.push(closed[0]);
            }
        }
    }
    None
}

/// Order corners TL, TR, BR, BL the way the stock driver does: the two leftmost by X are the
/// left side (smaller Y on top), the two rightmost the right side.
pub fn order_corners(q: [P2; 4]) -> [P2; 4] {
    let mut v = q.to_vec();
    v.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
    let (tl, bl) = if v[0][1] < v[1][1] {
        (v[0], v[1])
    } else {
        (v[1], v[0])
    };
    let (tr, br) = if v[2][1] < v[3][1] {
        (v[2], v[3])
    } else {
        (v[3], v[2])
    };
    [tl, tr, br, bl]
}

/// Push a half-resolution corner back to full resolution: within a small window, take the
/// bright pixel that lies farthest along the direction from the quad centroid to the corner.
pub fn refine_corner(
    luma: &[u8],
    w: usize,
    h: usize,
    corner_half: P2,
    centroid_half: P2,
    threshold: u8,
    radius: i32,
) -> P2 {
    let cx = corner_half[0] * 2.0 + 0.5;
    let cy = corner_half[1] * 2.0 + 0.5;
    let dir = [
        corner_half[0] - centroid_half[0],
        corner_half[1] - centroid_half[1],
    ];
    let mut best = [cx, cy];
    let mut best_score = f64::NEG_INFINITY;
    #[allow(clippy::cast_possible_truncation)]
    let (ix, iy) = (cx.round() as i32, cy.round() as i32);
    let (wi, hi) = (
        i32::try_from(w).unwrap_or(i32::MAX),
        i32::try_from(h).unwrap_or(i32::MAX),
    );
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let (x, y) = (ix + dx, iy + dy);
            if x < 0 || y < 0 || x >= wi || y >= hi {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let v = luma[y as usize * w + x as usize];
            if v < threshold {
                continue;
            }
            let p = [f64::from(x), f64::from(y)];
            let score = (p[0] - cx) * dir[0] + (p[1] - cy) * dir[1];
            if score > best_score {
                best_score = score;
                best = p;
            }
        }
    }
    best
}

/// A detected border quad, corners at full resolution ordered TL, TR, BR, BL.
#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub corners: [P2; 4],
    /// Half-resolution blob bounding box, for diagnostics.
    pub blob: Blob,
    /// True if the blob touches the frame edge (the border is clipped; corners are unreliable).
    pub clipped: bool,
}

impl Quad {
    /// Map from full-resolution camera pixels to screen percent.
    pub fn to_screen(&self) -> Mat3 {
        quad_to_quad(&self.corners, &SCREEN_PERCENT)
    }

    pub fn centroid(&self) -> P2 {
        let c = &self.corners;
        [
            (c[0][0] + c[1][0] + c[2][0] + c[3][0]) / 4.0,
            (c[0][1] + c[1][1] + c[2][1] + c[3][1]) / 4.0,
        ]
    }
}

/// Run the whole acquisition on a full-resolution luma frame.
pub fn acquire(luma: &[u8], w: usize, h: usize, p: &AcquireParams) -> Option<Quad> {
    let mask = decimate_threshold(luma, w, h, p.threshold);
    let (labels, blobs) = label(&mask);
    for blob in blobs.iter().take(8) {
        let min = p.min_size as usize;
        if blob.width() < min || blob.height() < min {
            continue;
        }
        let pts = boundary_points(&labels, mask.w, mask.h, blob);
        let hull = convex_hull(&pts);
        let Some(q) = approx_quad(&hull) else {
            continue;
        };
        let q = order_corners(q);
        #[allow(clippy::cast_precision_loss)]
        let bbox_area = (blob.width() * blob.height()) as f64;
        let quad_area = 0.5
            * ((q[0][0] * q[1][1] - q[1][0] * q[0][1])
                + (q[1][0] * q[2][1] - q[2][0] * q[1][1])
                + (q[2][0] * q[3][1] - q[3][0] * q[2][1])
                + (q[3][0] * q[0][1] - q[0][0] * q[3][1]))
                .abs();
        if quad_area < 0.5 * bbox_area {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let centroid = [
            (q[0][0] + q[1][0] + q[2][0] + q[3][0]) / 4.0,
            (q[0][1] + q[1][1] + q[2][1] + q[3][1]) / 4.0,
        ];
        let corners: [P2; 4] = core::array::from_fn(|i| {
            refine_corner(luma, w, h, q[i], centroid, p.threshold, p.refine_radius)
        });
        let clipped =
            blob.x0 == 0 || blob.y0 == 0 || blob.x1 == mask.w - 1 || blob.y1 == mask.h - 1;
        return Some(Quad {
            corners,
            blob: *blob,
            clipped,
        });
    }
    None
}

/// Image flips applied before detection, to make the camera frame match screen orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flip {
    None,
    /// Mirror left-right.
    Horizontal,
    /// Mirror top-bottom.
    Vertical,
    /// Rotate 180 degrees. What the Sinden camera board needs: it is mounted upside down
    /// (observed: the cursor moved opposite to the gun on both axes without it, matching the
    /// vendor driver's "camera is upside down" sign).
    Both,
}

/// Apply `flip` to a luma frame in place.
pub fn flip_luma(luma: &mut [u8], w: usize, flip: Flip) {
    match flip {
        Flip::None => {}
        Flip::Horizontal => luma.chunks_exact_mut(w).for_each(<[u8]>::reverse),
        Flip::Vertical => {
            let h = luma.len() / w;
            for y in 0..h / 2 {
                let (a, b) = luma.split_at_mut((h - 1 - y) * w);
                a[y * w..y * w + w].swap_with_slice(&mut b[..w]);
            }
        }
        Flip::Both => luma.reverse(),
    }
}

/// The stock aim model: the camera pixel the bore points at, given the bore offset in percent
/// of frame and the camera orientation sign (-1 for modern boards, +1 for legacy).
pub fn aim_pixel(
    w: usize,
    h: usize,
    cal_x_percent: f64,
    cal_y_percent: f64,
    orientation: f64,
) -> P2 {
    #[allow(clippy::cast_precision_loss)]
    let (fw, fh) = (w as f64, h as f64);
    [
        fw / 2.0 + cal_x_percent / 100.0 * fw * orientation,
        fh / 2.0 + cal_y_percent / 100.0 * fh * orientation,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draw a hollow quad border (white ring) into a black luma frame.
    fn synth(w: usize, h: usize, outer: &[P2; 4], inset: f64) -> Vec<u8> {
        let inner: [P2; 4] = {
            let c = [
                (outer[0][0] + outer[1][0] + outer[2][0] + outer[3][0]) / 4.0,
                (outer[0][1] + outer[1][1] + outer[2][1] + outer[3][1]) / 4.0,
            ];
            core::array::from_fn(|i| {
                [
                    c[0] + (outer[i][0] - c[0]) * inset,
                    c[1] + (outer[i][1] - c[1]) * inset,
                ]
            })
        };
        let inside = |q: &[P2; 4], p: P2| (0..4).all(|i| cross(q[i], q[(i + 1) % 4], p) >= 0.0);
        let mut img = vec![10u8; w * h];
        for y in 0..h {
            for x in 0..w {
                #[allow(clippy::cast_precision_loss)]
                let p = [x as f64, y as f64];
                if inside(outer, p) && !inside(&inner, p) {
                    img[y * w + x] = 230;
                }
            }
        }
        img
    }

    #[test]
    fn finds_synthetic_border() {
        let (w, h) = (640, 480);
        let truth = [[95.0, 70.0], [548.0, 52.0], [566.0, 415.0], [78.0, 398.0]];
        let img = synth(w, h, &truth, 0.94);
        let q = acquire(&img, w, h, &AcquireParams::default()).expect("border found");
        assert!(!q.clipped);
        for (i, (got, want)) in q.corners.iter().zip(truth.iter()).enumerate() {
            let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
            assert!(d <= 2.5, "corner {i}: got {got:?} want {want:?} (d={d:.2})");
        }
        // Aim from the true centre of the screen quad should land near 50%,50%.
        let m = q.to_screen();
        let centre_cam = quad_to_quad(&SCREEN_PERCENT, &truth)
            .apply([50.0, 50.0])
            .expect("finite");
        let aim = m.apply(centre_cam).expect("finite");
        assert!(
            (aim[0] - 50.0).abs() < 1.0 && (aim[1] - 50.0).abs() < 1.0,
            "{aim:?}"
        );
    }

    #[test]
    fn flips() {
        let mut a = vec![1, 2, 3, 4, 5, 6];
        flip_luma(&mut a, 3, Flip::Horizontal);
        assert_eq!(a, [3, 2, 1, 6, 5, 4]);
        flip_luma(&mut a, 3, Flip::Vertical);
        assert_eq!(a, [6, 5, 4, 3, 2, 1]);
        flip_luma(&mut a, 3, Flip::Both);
        assert_eq!(a, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn rejects_empty_frame() {
        let img = vec![5u8; 640 * 480];
        assert!(acquire(&img, 640, 480, &AcquireParams::default()).is_none());
    }

    #[test]
    fn hull_and_quad_of_rectangle() {
        let pts: Vec<P2> = (0..50)
            .flat_map(|i| {
                let t = f64::from(i);
                vec![[t, 0.0], [t, 30.0], [0.0, t.min(30.0)], [49.0, t.min(30.0)]]
            })
            .collect();
        let hull = convex_hull(&pts);
        assert!(hull.len() >= 4);
        let q = approx_quad(&hull).expect("quad");
        let q = order_corners(q);
        assert_eq!(q, [[0.0, 0.0], [49.0, 0.0], [49.0, 30.0], [0.0, 30.0]]);
    }
}
