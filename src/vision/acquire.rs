//! Border acquisition: find the bright quadrilateral (the screen border) in a luma frame.
//!
//! Threshold and 2x2 decimate, label connected components, take each large blob's boundary,
//! undistort it, fit straight lines to it and intersect the outermost line on each side to
//! get the corners. Because a line is pinned by any visible stretch of it, this works when
//! the corners are outside the frame as long as all four edges cross it. When fewer than
//! four sides are visible it falls back to the convex-hull quad of the blob, which is only
//! honest if the blob is not clipped; a clipped hull quad is flagged as such.

use super::code::{self, Side};
use super::homography::{fit_dlt, quad_to_quad, Mat3, L3, P2, SCREEN_PERCENT};
use super::lens::Lens;
use super::lines::{extract_lines, Line, Segment};

#[derive(Clone, Copy, Debug)]
pub struct AcquireParams {
    /// Luma threshold (0..=255) for "border" at full resolution.
    pub threshold: u8,
    /// Minimum blob width and height, in half-resolution pixels.
    pub min_size: u32,
    /// Corner-refinement search radius at full resolution, in pixels (hull fallback only).
    pub refine_radius: i32,
    /// Radial lens distortion coefficient; see [`Lens`].
    pub lens_k1: f64,
    /// Line-fit inlier tolerance at full resolution, in pixels.
    pub line_tol: f64,
    /// Minimum boundary points supporting a line, in half-resolution pixels.
    pub line_min_points: usize,
    /// Refit edge lines to sub-pixel luma crossings. Off by default: on the recordings it
    /// solved slightly fewer frames and spiked more often than the mask lines, and made
    /// no difference to hover jitter (which is the hand, not the fit).
    pub subpixel_lines: bool,
    /// Re-measure each tab's width from the luma profile through the tab bodies.
    pub subpixel_tabs: bool,
    /// Screen width over height, and the border thickness as a fraction of the shorter
    /// screen dimension: where the drawn inner edge and tab tips lie in screen percent,
    /// which lets a single visible side solve near that side.
    pub screen_aspect: f64,
    pub border_frac: f64,
}

impl Default for AcquireParams {
    fn default() -> Self {
        Self {
            threshold: 128,
            min_size: 20,
            refine_radius: 3,
            lens_k1: 0.0,
            line_tol: 3.0,
            line_min_points: 20,
            subpixel_lines: false,
            subpixel_tabs: true,
            screen_aspect: 16.0 / 9.0,
            border_frac: 0.03,
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

/// A detected border quad, corners in undistorted full-resolution pixels ordered TL, TR,
/// BR, BL.
#[derive(Clone, Copy, Debug)]
pub struct Quad {
    pub corners: [P2; 4],
    /// Half-resolution blob bounding box, for diagnostics.
    pub blob: Blob,
    /// True if the corners are unreliable: they came from the blob's hull rather than from
    /// four fitted edge lines, so the border is clipped or malformed. Good for showing where
    /// the border roughly is, not for aiming.
    pub clipped: bool,
    /// True if the corners are intersections of fitted edge lines (false: hull fallback).
    pub from_lines: bool,
    /// Decoded border tabs that went into the solve. Zero on a four-line solve means the
    /// tabs were unreadable (far away) or absent (not our border).
    pub tabs: u8,
    /// Which sides had an outer edge line, one bit per [`Side`] index.
    pub sides: u8,
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

/// Which screen edge a boundary line belongs to, from its outward normal.
fn side_of(l: &Line) -> Side {
    if l.b.abs() >= l.a.abs() {
        if l.b < 0.0 {
            Side::Top
        } else {
            Side::Bottom
        }
    } else if l.a < 0.0 {
        Side::Left
    } else {
        Side::Right
    }
}

/// Half-resolution boundary points pooled over every sizeable blob (long in at least one
/// dimension), plus the first such blob. Line fitting works on the pool because a border
/// with its corners off frame breaks into one strip per edge.
#[must_use]
pub fn pooled_boundary(
    labels: &[u32],
    mask_w: usize,
    mask_h: usize,
    blobs: &[Blob],
    min_size: usize,
) -> (Vec<P2>, Option<Blob>) {
    let mut pooled: Vec<P2> = Vec::new();
    let mut first: Option<Blob> = None;
    for blob in blobs.iter().take(8) {
        if blob.width().max(blob.height()) < min_size || (blob.area as usize) < 2 * min_size {
            continue;
        }
        first.get_or_insert(*blob);
        pooled.extend(boundary_points(labels, mask_w, mask_h, blob));
    }
    (pooled, first)
}

/// Undistorted full-resolution boundary points and the lines fitted through them.
pub struct Edges<'a> {
    pub points: Vec<P2>,
    /// Per point: it was within a few pixels of the frame edge, so anything it belongs to
    /// may be cut off there.
    pub near_edge: Vec<bool>,
    pub segments: Vec<Segment>,
    /// Each segment's line with its normal pointing from the bright side to the dark side,
    /// decided by sampling the mask on both sides of its inliers.
    pub outward: Vec<Line>,
    /// The mask and lens, so later stages can ask whether an undistorted full-resolution
    /// point is bright.
    pub mask: Mask,
    pub lens: Lens,
    /// The full-resolution luma frame, when the caller can lend it: sub-pixel edge and tab
    /// measurements read it directly instead of the half-resolution mask.
    pub luma: Option<&'a [u8]>,
    pub w: usize,
    pub h: usize,
    pub subpixel_lines: bool,
    pub subpixel_tabs: bool,
    pub screen_aspect: f64,
    pub border_frac: f64,
}

impl Edges<'_> {
    /// Bilinear luma at an undistorted full-resolution point, or `None` outside the frame
    /// or without a luma frame.
    #[must_use]
    pub fn sample(&self, p: P2) -> Option<f64> {
        let luma = self.luma?;
        let d = self.lens.distort(p);
        if !(d[0] >= 0.0 && d[1] >= 0.0) {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x0, y0) = (d[0].floor() as usize, d[1].floor() as usize);
        if x0 + 1 >= self.w || y0 + 1 >= self.h {
            return None;
        }
        let (fx, fy) = (d[0] - d[0].floor(), d[1] - d[1].floor());
        let at = |x: usize, y: usize| f64::from(luma[y * self.w + x]);
        Some(
            at(x0, y0) * (1.0 - fx) * (1.0 - fy)
                + at(x0 + 1, y0) * fx * (1.0 - fy)
                + at(x0, y0 + 1) * (1.0 - fx) * fy
                + at(x0 + 1, y0 + 1) * fx * fy,
        )
    }

    /// Is an undistorted point within a few pixels of the frame edge?
    #[must_use]
    pub fn near_frame_edge(&self, p: P2) -> bool {
        let d = self.lens.distort(p);
        #[allow(clippy::cast_precision_loss)]
        let (w, h) = (self.w as f64, self.h as f64);
        d[0] < 4.0 || d[1] < 4.0 || d[0] > w - 5.0 || d[1] > h - 5.0
    }

    /// Refit `line` (normal towards the dark side, through boundary pixel centres) to
    /// sub-pixel edge positions: at points along `span` the luma profile across the edge
    /// is read and the half-way crossing between the bright and dark levels located by
    /// interpolation. `None` without luma or with too few clean crossings.
    #[must_use]
    pub fn refine_line(&self, line: &Line, span: (f64, f64), dir: f64) -> Option<Line> {
        if !self.subpixel_lines {
            return None;
        }
        self.luma?;
        let mut pts: Vec<P2> = Vec::new();
        let mut t = span.0;
        while t <= span.1 {
            let base = line.point_at(dir * t);
            t += 3.0;
            let prof: Option<Vec<f64>> = (-6..=6)
                .map(|k| {
                    let k = f64::from(k);
                    self.sample([base[0] + line.a * k, base[1] + line.b * k])
                })
                .collect();
            let Some(v) = prof else { continue };
            let bright = v[..6].iter().copied().fold(f64::MIN, f64::max);
            let dark = v[7..].iter().copied().fold(f64::MAX, f64::min);
            if bright - dark < 30.0 {
                continue;
            }
            let half = (bright + dark) / 2.0;
            // First crossing below half, walking from the bright side out.
            for k in 0..12 {
                if v[k] >= half && v[k + 1] < half {
                    let frac = (v[k] - half) / (v[k] - v[k + 1]);
                    #[allow(clippy::cast_precision_loss)]
                    let s = (k as f64 - 6.0) + frac;
                    pts.push([base[0] + line.a * s, base[1] + line.b * s]);
                    break;
                }
            }
        }
        if pts.len() < 8 {
            return None;
        }
        let first = Line::fit(&pts)?;
        let kept: Vec<P2> = pts
            .iter()
            .copied()
            .filter(|p| first.signed_dist(*p).abs() <= 1.5)
            .collect();
        if kept.len() < 8 {
            return None;
        }
        let fit = Line::fit(&kept)?;
        // Keep the normal pointing the way the input's did.
        Some(if fit.a * line.a + fit.b * line.b < 0.0 {
            Line {
                a: -fit.a,
                b: -fit.b,
                c: -fit.c,
            }
        } else {
            fit
        })
    }
    /// Is the undistorted full-resolution point `p` on a bright mask pixel?
    #[must_use]
    pub fn bright_at(&self, p: P2) -> bool {
        let d = self.lens.distort(p);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x, y) = (
            ((d[0] - 0.5) / 2.0).round() as i64,
            ((d[1] - 0.5) / 2.0).round() as i64,
        );
        match (usize::try_from(x), usize::try_from(y)) {
            (Ok(x), Ok(y)) => {
                x < self.mask.w && y < self.mask.h && self.mask.bits[y * self.mask.w + x] != 0
            }
            _ => false,
        }
    }
}

/// Fit straight lines to pooled half-resolution boundary points. Points on the frame edge
/// are discarded (they are where the border leaves the frame, not an edge of it); the rest
/// are pushed to full resolution, undistorted, and handed to the line extractor.
#[must_use]
pub fn edge_segments<'a>(
    pts: &[P2],
    mask: &Mask,
    lens: &Lens,
    p: &AcquireParams,
    luma: Option<&'a [u8]>,
) -> Edges<'a> {
    #[allow(clippy::cast_precision_loss)]
    let (xe, ye) = ((mask.w - 1) as f64, (mask.h - 1) as f64);
    let kept: Vec<P2> = pts
        .iter()
        .copied()
        .filter(|q| q[0] > 0.0 && q[1] > 0.0 && q[0] < xe && q[1] < ye)
        .collect();
    let near_edge: Vec<bool> = kept
        .iter()
        .map(|q| q[0] <= 2.0 || q[1] <= 2.0 || q[0] >= xe - 2.0 || q[1] >= ye - 2.0)
        .collect();
    let points: Vec<P2> = kept
        .iter()
        .map(|q| lens.undistort([q[0] * 2.0 + 0.5, q[1] * 2.0 + 0.5]))
        .collect();
    let segments = if points.len() < 4 * p.line_min_points {
        Vec::new()
    } else {
        // Room for four outer and four inner edges plus the lines the tab tips form.
        extract_lines(&points, p.line_tol, p.line_min_points, 12)
    };
    let bright = |q: P2| -> bool {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (x, y) = (q[0].round() as i64, q[1].round() as i64);
        match (usize::try_from(x), usize::try_from(y)) {
            (Ok(x), Ok(y)) => x < mask.w && y < mask.h && mask.bits[y * mask.w + x] != 0,
            _ => false,
        }
    };
    let outward = segments
        .iter()
        .map(|seg| {
            // Lens distortion barely rotates a normal over a few pixels, so the undistorted
            // normal serves for sampling the distorted mask.
            let (a, b) = (seg.line.a, seg.line.b);
            let (mut plus, mut minus) = (0i32, 0i32);
            for &k in seg.idx.iter().step_by(3) {
                let q = kept[k];
                if bright([q[0] + 1.5 * a, q[1] + 1.5 * b]) {
                    plus += 1;
                }
                if bright([q[0] - 1.5 * a, q[1] - 1.5 * b]) {
                    minus += 1;
                }
            }
            // Normal towards the dark side.
            if plus > minus {
                Line {
                    a: -a,
                    b: -b,
                    c: -seg.line.c,
                }
            } else {
                seg.line
            }
        })
        .collect();
    Edges {
        points,
        near_edge,
        segments,
        outward,
        mask: Mask {
            w: mask.w,
            h: mask.h,
            bits: mask.bits.clone(),
        },
        lens: *lens,
        luma,
        w: mask.w * 2,
        h: mask.h * 2,
        subpixel_lines: p.subpixel_lines,
        subpixel_tabs: p.subpixel_tabs,
        screen_aspect: p.screen_aspect,
        border_frac: p.border_frac,
    }
}

/// The lines found for one side.
#[derive(Clone, Copy, Debug)]
pub struct SideLines {
    /// Outer edge, normal outward, pushed to the outer edge of the boundary pixels.
    pub outer: Line,
    pub outer_seg: usize,
    /// The inner edge's segment, if it was found as a line too.
    pub inner_seg: Option<usize>,
    /// Distance from the outer edge to the inner edge in image pixels.
    pub thickness: Option<f64>,
    /// Sign that makes the along-line coordinate increase with the screen coordinate
    /// (rightwards on horizontal sides, downwards on vertical ones).
    pub dir: f64,
    /// Extent of the side along the outer line (screen-ordered coordinates), from the
    /// outer and inner segments and any leftover segment collinear with the outer.
    pub span: (f64, f64),
    /// The inner edge and the line the tab tips form, at their true positions and with
    /// the outer's orientation, when fitted. Drawn lines at known screen offsets, so each
    /// is a line correspondence of its own.
    pub inner: Option<Line>,
    pub tips: Option<Line>,
}

impl SideLines {
    /// Coordinate of `p` along the outer line, increasing with the screen coordinate.
    #[must_use]
    pub fn along(&self, p: P2) -> f64 {
        self.dir * self.outer.along(p)
    }

    /// The point on the outer line at screen-ordered coordinate `t`.
    #[must_use]
    pub fn point_at(&self, t: f64) -> P2 {
        self.outer.point_at(self.dir * t)
    }
}

/// Assign fitted lines to sides.
///
/// Every edge of the border comes as a bright-to-dark line, so the outer and inner edge of
/// one side are a close, parallel pair with opposite normals, and locally nothing tells them
/// apart: both look out onto darkness. What breaks the symmetry is the tabs, which sit only
/// on the inner edge, so the member of a pair with tab-like boundary points just beyond its
/// partner is the outer edge. Without tab evidence (far away, tabs unresolved) the member
/// farther from the boundary centroid is taken as outer, which holds whenever the ring
/// encloses the centroid, i.e. when all four sides are in view. The centroid alone was the
/// old rule, and with three sides in view it turned the left border's inner edge into a
/// confident, wrong right edge.
#[must_use]
pub fn classify_sides(edges: &Edges) -> [Option<SideLines>; 4] {
    let segs = &edges.segments;
    let all: usize = segs.iter().map(|s| s.inliers.len()).sum();
    if all == 0 {
        return [None; 4];
    }
    #[allow(clippy::cast_precision_loss)]
    let n = all as f64;
    let centroid = segs
        .iter()
        .flat_map(|s| s.inliers.iter())
        .fold([0.0, 0.0], |a, q| [a[0] + q[0] / n, a[1] + q[1] / n]);
    let outward = &edges.outward;
    let mut member = vec![false; edges.points.len()];
    for s in segs {
        for &k in &s.idx {
            member[k] = true;
        }
    }
    let span = |k: usize| -> (f64, f64) {
        let l = &outward[k];
        segs[k]
            .inliers
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), p| {
                let a = l.along(*p);
                (lo.min(a), hi.max(a))
            })
    };
    // Tab-like evidence for `k` being an outer edge of thickness `t`: boundary points on no
    // line, in the band the tabs occupy beyond the inner edge, within the line's span.
    let evidence = |k: usize, t: f64| -> usize {
        let l = &outward[k];
        let (lo, hi) = span(k);
        edges
            .points
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                if member[*i] {
                    return false;
                }
                let inward = -l.signed_dist(**p);
                let a = l.along(**p);
                let (blo, bhi) = tab_band(t);
                inward >= blo && inward <= bhi && a >= lo && a <= hi
            })
            .count()
    };
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by_key(|&k| std::cmp::Reverse(segs[k].inliers.len()));
    let mut used = vec![false; segs.len()];
    let mut out: [Option<SideLines>; 4] = [None; 4];
    for &k in &order {
        if used[k] {
            continue;
        }
        let l = outward[k];
        // Nearest parallel line with the opposite normal on this line's bright side.
        let partner = order
            .iter()
            .copied()
            .filter(|&m| m != k && !used[m])
            .filter_map(|m| {
                let o = &outward[m];
                if l.cos_angle(o) < 0.985 || l.a * o.a + l.b * o.b > -0.9 {
                    return None;
                }
                let mid = segs[m].inliers[segs[m].inliers.len() / 2];
                let d = -l.signed_dist(mid);
                (d > 3.0 && d < 80.0).then_some((m, d))
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        // Every boundary point lies inside the outer quad, so a real outer edge always has
        // the centroid on its bright side; an inner edge, or the line the tab tips form,
        // has it on the dark side.
        let faces_in = |k: usize| outward[k].signed_dist(centroid) < 0.0;
        // Nearest parallel line with the opposite normal on `from`'s bright side.
        let nearest_partner = |from: usize, used: &[bool]| -> Option<(usize, f64)> {
            let l = &outward[from];
            order
                .iter()
                .copied()
                .filter(|&m| m != from && !used[m])
                .filter_map(|m| {
                    let o = &outward[m];
                    if l.cos_angle(o) < 0.985 || l.a * o.a + l.b * o.b > -0.9 {
                        return None;
                    }
                    let mid = segs[m].inliers[segs[m].inliers.len() / 2];
                    let d = -l.signed_dist(mid);
                    if !(d > 3.0 && d < 80.0) {
                        return None;
                    }
                    // The border between an outer edge and its inner edge is solid; between
                    // an outer edge and the line the tab tips form it is bright only at the
                    // tabs. Sample a few pixels on the outer side of the candidate along
                    // its span (the midline sits inside the solid border either way).
                    // Sampled uniformly along the span, not at the inliers: the tip line's
                    // inliers are the tabs themselves and would always read bright.
                    let lm = &outward[m];
                    let (lo, hi) =
                        segs[m]
                            .inliers
                            .iter()
                            .fold((f64::MAX, f64::MIN), |(lo, hi), p| {
                                let a = lm.along(*p);
                                (lo.min(a), hi.max(a))
                            });
                    let step = (d / 2.0).min(4.0);
                    let (mut lit, mut n) = (0usize, 0usize);
                    let mut t = lo;
                    while t <= hi {
                        let p = lm.point_at(t);
                        n += 1;
                        if edges.bright_at([p[0] + l.a * step, p[1] + l.b * step]) {
                            lit += 1;
                        }
                        t += 4.0;
                    }
                    (n > 0 && lit * 10 >= n * 8).then_some((m, d))
                })
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        };
        let outer_k = match partner {
            Some((m, d)) => {
                let (ek, em) = (evidence(k, d), evidence(m, d));
                let k_outer = if ek.max(em) >= 6 {
                    ek >= em
                } else {
                    -l.signed_dist(centroid) >= -outward[m].signed_dist(centroid)
                };
                let (o, i) = if k_outer { (k, m) } else { (m, k) };
                if faces_in(o) {
                    o
                } else if faces_in(i) && ek.max(em) < 6 {
                    i
                } else {
                    used[k] = true;
                    continue;
                }
            }
            None => {
                if !faces_in(k) {
                    continue;
                }
                k
            }
        };
        // The line the tab tips form is parallel too and, being long, may have been the
        // partner found first; the inner edge is whatever lies nearest inside the outer.
        let inner = nearest_partner(outer_k, &used);
        let outer = outward[outer_k];
        let side = side_of(&outer);
        if out[side as usize].is_some() {
            continue;
        }
        used[outer_k] = true;
        if let Some((m, _)) = inner {
            used[m] = true;
        }
        let d = outer.direction();
        let forward = if side.is_horizontal() { d[0] } else { d[1] };
        let dir = if forward >= 0.0 { 1.0 } else { -1.0 };
        let mut span = (f64::MAX, f64::MIN);
        for (m, seg) in segs.iter().enumerate() {
            let structural = m == outer_k || inner.is_some_and(|i| i.0 == m);
            let collinear = !used[m]
                && outward[m].cos_angle(&outer) > 0.995
                && seg
                    .inliers
                    .iter()
                    .all(|p| outer.signed_dist(*p).abs() < 4.0);
            if !(structural || collinear) {
                continue;
            }
            for p in &seg.inliers {
                let a = dir * outer.along(*p);
                span = (span.0.min(a), span.1.max(a));
            }
        }
        // Sub-pixel edges where the luma is available; otherwise the outer edge of a
        // boundary pixel is one full-resolution pixel beyond its centre.
        let refined_outer = edges.refine_line(&outer, span, dir);
        let outer_line = refined_outer.unwrap_or_else(|| outer.offset(1.0));
        let mut thickness = inner.map(|(m, d)| {
            let inner_line = outward[m];
            match (refined_outer, edges.refine_line(&inner_line, span, dir)) {
                (Some(o), Some(i)) => {
                    let mid = i.point_at(dir * (span.0 + span.1) / 2.0);
                    (-o.signed_dist(mid)).max(2.0)
                }
                _ => d + 2.0,
            }
        });
        // The mask itself says how thick the border is here: the median bright run inward
        // from the outer edge. It overrides a partner that sits much further in (the line
        // the tab tips form, when the inner edge was not fitted or the strip test let it
        // through) and supplies a thickness when no inner line was found at all.
        if let Some(walked) = walk_thickness(edges, &outer_line, dir, span) {
            match thickness {
                Some(t) if t <= 1.6 * walked => {}
                _ => thickness = Some(walked),
            }
        }
        // The inner edge as a line, and the tab tips' line: parallel, on the bright side,
        // at about one and two thicknesses. Boundary pixel centres sit one pixel short of
        // the true edge; flip to the outer's orientation.
        let to_outer_orientation = |l: Line| Line {
            a: -l.a,
            b: -l.b,
            c: -(l.c + 1.0),
        };
        let inner_line = inner.map(|(m, _)| to_outer_orientation(outward[m]));
        let tips_line = thickness.and_then(|t| {
            order
                .iter()
                .copied()
                .filter(|&m| !used[m] && m != outer_k && inner.is_none_or(|i| i.0 != m))
                .filter_map(|m| {
                    let o = &outward[m];
                    if outer.cos_angle(o) < 0.985 || outer.a * o.a + outer.b * o.b > -0.9 {
                        return None;
                    }
                    let mid = segs[m].inliers[segs[m].inliers.len() / 2];
                    let d = -outer_line.signed_dist(mid) + 1.0;
                    (d > 1.5 * t && d < 2.7 * t).then_some((m, (d - 2.0 * t).abs()))
                })
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(m, _)| to_outer_orientation(outward[m]))
        });
        // Everything parallel within three thicknesses inside this edge is this side's own
        // structure (inner edge, tab tips, a split segment of either) and must not go on to
        // be classified as another side: with one border in view edge-on, the tab-tip line
        // became a bogus opposite side whose corner exclusion erased every real tab.
        if let Some(t) = thickness {
            for m in 0..segs.len() {
                if used[m] || outward[m].cos_angle(&outer) < 0.985 {
                    continue;
                }
                let mid = segs[m].inliers[segs[m].inliers.len() / 2];
                let d = -outer_line.signed_dist(mid);
                if d > 0.0 && d < 3.2 * t {
                    used[m] = true;
                }
            }
        }
        out[side as usize] = Some(SideLines {
            outer: outer_line,
            outer_seg: outer_k,
            inner_seg: inner.map(|i| i.0),
            thickness,
            dir,
            span,
            inner: inner_line,
            tips: tips_line,
        });
    }
    out
}

/// Median bright run inward from `outer` (normal outward, at the true edge) at points
/// every few pixels along `span`, in full-resolution pixels. Runs are measured between
/// mask pixel centres, so one pixel is added for the half pixel at each end. `None` if
/// too few samples found any border at all.
fn walk_thickness(edges: &Edges, outer: &Line, dir: f64, span: (f64, f64)) -> Option<f64> {
    let mut runs = Vec::new();
    let mut t = span.0;
    while t <= span.1 {
        let base = outer.point_at(dir * t);
        let mut run = 0.0;
        let mut d = 1.0;
        while d < 80.0 {
            let p = [base[0] - outer.a * d, base[1] - outer.b * d];
            if !edges.bright_at(p) {
                break;
            }
            run = d;
            d += 1.0;
        }
        if run > 0.0 {
            runs.push(run + 1.0);
        }
        t += 6.0;
    }
    if runs.len() < 4 {
        return None;
    }
    runs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(runs[runs.len() / 2])
}

/// The band of inward distances from a side's outer edge that its tab walls and tips
/// occupy, given the pixel distance `d` between the outer and inner boundary lines. Tabs
/// run from the inner edge one thickness inward; the band starts just past the inner
/// boundary pixels and reaches past the tips with room for a thickness mis-estimate.
fn tab_band(d: f64) -> (f64, f64) {
    (1.1 * d + 2.0, 2.7 * d + 4.0)
}

/// A tab seen on one side: its centre and width along the outer line, in pixels.
#[derive(Clone, Copy, Debug)]
pub struct SeenTab {
    pub along: f64,
    pub width: f64,
    /// Cut off by the frame edge: the width is meaningless.
    pub partial: bool,
}

/// Find the tabs on a side: boundary points that belong to no side's outer or inner edge
/// and sit in the band just inside this side's inner edge, clustered along its outer line.
/// (The tab tips are collinear and get a fitted line of their own, so membership of any
/// line would not do.)
#[must_use]
pub fn find_tabs(edges: &Edges, sides: &[Option<SideLines>; 4], side: &SideLines) -> Vec<SeenTab> {
    let Some(t) = side.thickness else {
        return Vec::new();
    };
    let mut member = vec![false; edges.points.len()];
    for sl in sides.iter().flatten() {
        for seg in [Some(sl.outer_seg), sl.inner_seg].into_iter().flatten() {
            for &k in &edges.segments[seg].idx {
                member[k] = true;
            }
        }
    }
    // Points on any line crossing this side (another side's edges, whether or not that
    // side was classified) are structure, not tabs. Lines parallel to this side stay
    // available: the tab tips form one.
    for (i, seg) in edges.segments.iter().enumerate() {
        if edges.outward[i].cos_angle(&side.outer) < 0.87 {
            for &k in &seg.idx {
                member[k] = true;
            }
        }
    }
    let outer = &side.outer;
    let (span_lo, span_hi) = side.span;
    // The other sides' own tabs sit in this band near the corners; keep clear of them.
    let others: Vec<(Line, f64)> = sides
        .iter()
        .flatten()
        .filter(|o| o.outer_seg != side.outer_seg)
        .map(|o| (o.outer, o.thickness.unwrap_or(t)))
        .collect();
    let mut cand: Vec<(f64, bool)> = edges
        .points
        .iter()
        .enumerate()
        .filter(|(k, p)| {
            if member[*k] {
                return false;
            }
            let inward = -outer.signed_dist(**p);
            let a = side.along(**p);
            let (blo, bhi) = tab_band(t - 2.0);
            inward >= blo
                && inward <= bhi
                && a >= span_lo - t
                && a <= span_hi + t
                && others.iter().all(|(l, t2)| -l.signed_dist(**p) > 2.7 * t2)
        })
        .map(|(k, p)| (side.along(*p), edges.near_edge[k]))
        .collect();
    cand.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out = Vec::new();
    let mut i = 0;
    while i < cand.len() {
        let mut j = i + 1;
        while j < cand.len() && cand[j].0 - cand[j - 1].0 <= 0.6 * t {
            j += 1;
        }
        let n = j - i;
        // Boundary pixel centres sit half a pixel inside each wall (full-resolution pixels
        // are two mask pixels wide), hence the width correction.
        let width = cand[j - 1].0 - cand[i].0 + 2.0;
        let partial = cand[i..j].iter().any(|c| c.1);
        if n >= 3 && width >= 0.5 * t && width <= 4.5 * t {
            out.push(SeenTab {
                along: (cand[i].0 + cand[j - 1].0) / 2.0,
                width,
                partial,
            });
        }
        i = j;
    }
    if edges.subpixel_tabs && edges.luma.is_some() {
        for tab in &mut out {
            refine_tab(edges, side, t, tab);
        }
    }
    out
}

/// Re-measure a tab from the luma: a brightness profile along a line through the tab
/// bodies, one and a half thicknesses inside the outer edge, thresholded half-way between
/// the border's brightness and the interior, with both crossings interpolated. The
/// cluster's centre and width stand if the profile is unusable.
fn refine_tab(edges: &Edges, side: &SideLines, t: f64, tab: &mut SeenTab) {
    let outer = &side.outer;
    let step = 0.5;
    let half_w = tab.width / 2.0 + 0.6 * t;
    let (lo, hi) = (tab.along - half_w, tab.along + half_w);
    let n = ((hi - lo) / step).floor().max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = n as usize;
    if n < 6 {
        return;
    }
    let at = |i: usize, inward: f64| -> P2 {
        #[allow(clippy::cast_precision_loss)]
        let p = side.point_at(lo + i as f64 * step);
        [p[0] - outer.a * inward, p[1] - outer.b * inward]
    };
    // Border brightness on the solid strip behind this tab; interior darkness beyond it.
    let mid = n / 2;
    let (Some(bright), Some(dark)) = (
        edges.sample(at(mid, 0.5 * t)),
        edges.sample(at(mid, 3.2 * t)),
    ) else {
        return;
    };
    if bright - dark < 30.0 {
        return;
    }
    let half = (bright + dark) / 2.0;
    let prof: Vec<Option<f64>> = (0..n).map(|i| edges.sample(at(i, 1.5 * t))).collect();
    // The bright run containing the centre.
    if !prof[mid].is_some_and(|v| v >= half) {
        return;
    }
    let mut a = mid;
    while a > 0 && prof[a - 1].is_some_and(|v| v >= half) {
        a -= 1;
    }
    let mut b = mid;
    while b + 1 < n && prof[b + 1].is_some_and(|v| v >= half) {
        b += 1;
    }
    if a == 0 || b + 1 >= n {
        // The run reaches the window's edge: not a clean tab here (or a cut-off one).
        tab.partial = true;
        return;
    }
    #[allow(clippy::cast_precision_loss)]
    let rise = match (prof[a - 1], prof[a]) {
        (Some(p), Some(q)) if q > p => (a - 1) as f64 + (half - p) / (q - p),
        _ => a as f64,
    };
    #[allow(clippy::cast_precision_loss)]
    let fall = match (prof[b], prof[b + 1]) {
        (Some(p), Some(q)) if p > q => b as f64 + (p - half) / (p - q),
        _ => (b + 1) as f64,
    };
    let width = (fall - rise) * step;
    if width < 0.4 * t || width > 5.0 * t {
        return;
    }
    tab.width = width;
    tab.along = lo + (rise + fall) / 2.0 * step;
}

/// Constant by which thresholding fattens a measured tab width, in full-resolution pixels.
pub const WIDTH_BIAS_PX: f64 = 1.6;

/// What decoding one side's tabs produced.
#[derive(Clone, Debug, Default)]
pub struct Decoded {
    /// (image point on the outer line, screen point) correspondences.
    pub points: Vec<(P2, P2)>,
    /// The side the code says this is, if any run identified it.
    pub side: Option<Side>,
}

/// Decode the tabs seen on an edge into (image point on the outer line, screen point)
/// correspondences. The local unit comes from the centre-to-centre spacing of whole
/// neighbouring tabs (constant pitch on screen), each width is read against it, and runs
/// of three or more confident symbols are looked up in the code, which names the side,
/// the position and the reading direction. `lines` is the edge as classified from its
/// normal; the code overrides that guess, which is what makes the solve roll-invariant.
///
/// With `known` (the side and whether this edge reads against that side's direction,
/// settled by the roll another edge revealed) runs of two symbols are placed as well.
#[must_use]
pub fn decode_tabs(
    lines: &SideLines,
    seen: &[SeenTab],
    known: Option<(Side, bool)>,
    width_bias: f64,
) -> Decoded {
    let mut out = Decoded::default();
    let Some(t) = lines.thickness else {
        return out;
    };
    // Split into runs wherever the spacing is not one pitch. The unit is about one
    // thickness on a 16:9 screen and three quarters of it on 4:3, and perspective can
    // squeeze or stretch a pitch by a third, while a missed tab doubles it.
    let mut runs: Vec<Vec<usize>> = Vec::new();
    for (k, tab) in seen.iter().enumerate() {
        let contiguous = k > 0 && {
            let d = tab.along - seen[k - 1].along;
            d >= 3.3 * t && d <= 8.5 * t
        };
        if contiguous {
            if let Some(r) = runs.last_mut() {
                r.push(k);
                continue;
            }
        }
        runs.push(vec![k]);
    }
    // Thresholding fattens every tab by about the same number of pixels whatever its size,
    // so widths carry a constant bias (measured +1.6 px on the recordings) while the unit,
    // from centre spacing, does not. Estimate the bias for this edge from its own tabs:
    // the value that makes their widths closest to whole units.
    let mut reads: Vec<(f64, f64)> = Vec::new(); // (width px, unit px)
    let unit_at = |run: &[usize], pos: usize| -> Option<f64> {
        let k = run[pos];
        let mut pitches = Vec::new();
        if pos > 0 && !seen[run[pos - 1]].partial {
            pitches.push(seen[k].along - seen[run[pos - 1]].along);
        }
        if pos + 1 < run.len() && !seen[run[pos + 1]].partial {
            pitches.push(seen[run[pos + 1]].along - seen[k].along);
        }
        if pitches.is_empty() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(pitches.iter().sum::<f64>() / pitches.len() as f64 / code::PITCH_UNITS)
    };
    for run in &runs {
        for pos in 0..run.len() {
            if seen[run[pos]].partial {
                continue;
            }
            if let Some(u) = unit_at(run, pos) {
                reads.push((seen[run[pos]].width, u));
            }
        }
    }
    let mut bias = width_bias;
    if reads.len() >= 4 {
        let cost = |b: f64| {
            reads
                .iter()
                .map(|&(w, u)| {
                    let x = (w - b) / u;
                    (x - x.round()).abs()
                })
                .sum::<f64>()
        };
        let mut best = (cost(bias), bias);
        let mut b = width_bias - 2.0;
        while b <= width_bias + 2.0 {
            let c = cost(b);
            if c < best.0 {
                best = (c, b);
            }
            b += 0.25;
        }
        bias = best.1;
    }
    for run in runs {
        let mut syms: Vec<(usize, u8)> = Vec::new();
        for (pos, &k) in run.iter().enumerate() {
            if seen[k].partial {
                continue;
            }
            let Some(unit) = unit_at(&run, pos) else {
                continue;
            };
            let w = (seen[k].width - bias) / unit;
            // A width near a decision boundary is a coin toss, and one misread symbol can
            // match the code somewhere else, so an uncertain tab ends the run.
            let max = f64::from(code::SYMBOLS);
            if (w - w.round()).abs() > 0.3 || !(0.6..=max + 0.4).contains(&w) {
                syms.push((k, u8::MAX));
                continue;
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let sym = (w.round() as i64 - 1).clamp(0, i64::from(code::SYMBOLS) - 1) as u8;
            syms.push((k, sym));
        }
        // Split at uncertain tabs; each confident stretch needs three symbols, the window
        // size the code makes unique, or two once the side and direction are known.
        let min_len = if known.is_some() { 2 } else { 3 };
        let stretches: Vec<Vec<(usize, u8)>> = syms
            .split(|s| s.1 == u8::MAX)
            .filter(|st| st.len() >= min_len)
            .map(<[(usize, u8)]>::to_vec)
            .collect();
        for syms in stretches {
            let symbols: Vec<u8> = syms.iter().map(|s| s.1).collect();
            let place = |r: &[u8]| -> Option<code::Placement> {
                match known {
                    Some((side, reversed)) => {
                        code::locate(side, r, reversed).map(|index| code::Placement {
                            side,
                            index,
                            reversed,
                        })
                    }
                    None => code::identify(r),
                }
            };
            // One misread symbol (a target ring touching a tab, a reflection) spoils the
            // whole stretch, so fall back to the longest sub-run that places itself. Short
            // sub-runs of a long stretch are held to four symbols: a misread three-window
            // has a fair chance of matching somewhere by accident.
            let mut found: Option<(usize, usize, code::Placement)> = None;
            let n = symbols.len();
            'search: for len in (min_len..=n).rev() {
                if len < n && len < 4 && n > 4 {
                    break;
                }
                for lo in 0..=n - len {
                    let Some(pl) = place(&symbols[lo..lo + len]) else {
                        continue;
                    };
                    // The whole stretch is contiguous tabs, so the symbols outside the
                    // placed sub-run must still fit on that side: a placement that puts
                    // tabs before the side's first tab or past its last is a misread
                    // sub-run matching another side by accident.
                    let total = code::tabs(pl.side).len();
                    let (before, after) = if pl.reversed {
                        (n - (lo + len), lo)
                    } else {
                        (lo, n - (lo + len))
                    };
                    if before > pl.index || pl.index + len + after > total {
                        continue;
                    }
                    found = Some((lo, lo + len, pl));
                    break 'search;
                }
            }
            let Some((lo, hi, pl)) = found else {
                continue;
            };
            if out.side.is_some_and(|s| s != pl.side) {
                // Two runs on one edge naming different sides: trust neither.
                out.points.clear();
                out.side = None;
                return out;
            }
            out.side = Some(pl.side);
            let table = code::tabs(pl.side);
            let n = hi - lo;
            for (i, &(k, _)) in syms[lo..hi].iter().enumerate() {
                let at = if pl.reversed {
                    pl.index + n - 1 - i
                } else {
                    pl.index + i
                };
                let Some(tab) = table.get(at) else {
                    break;
                };
                out.points.push((
                    lines.point_at(seen[k].along),
                    pl.side.outer_point(tab.centre()),
                ));
            }
        }
    }
    out
}

fn line3(l: &Line) -> L3 {
    [l.a, l.b, -l.c]
}

/// Everything the solve saw, for diagnostics.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub sides: [Option<SideLines>; 4],
    /// Tabs seen on each side, decoded or not.
    pub seen: [Vec<SeenTab>; 4],
    /// Tab correspondences decoded on each side.
    pub decoded: [usize; 4],
    /// Every (image point, screen point) correspondence used.
    pub points: Vec<(P2, P2)>,
    /// Why no corners came out, if they did not.
    pub refused: Option<&'static str>,
}

/// Solve the border from whatever sides and tabs are visible. With all four outer lines
/// the corners are their intersections; otherwise the lines and decoded tab points go
/// into a direct linear transform. Each line pins two degrees of freedom and a tab on a
/// known line one more, so two sides need four tabs and three sides need two.
/// Returns the corners and the number of tab correspondences used, plus the report.
#[must_use]
pub fn solve(edges: &Edges, mask_w: usize, mask_h: usize) -> (Option<([P2; 4], u8)>, Report) {
    let sides = classify_sides(edges);
    let mut report = Report {
        sides,
        ..Default::default()
    };
    let mut points: Vec<(P2, P2)> = Vec::new();
    // Decode each classified edge, then let the code say which side it really is: the
    // normal's guess fails past 45 degrees of roll, the tabs do not.
    let guessed = sides;
    let mut sides: [Option<SideLines>; 4] = [None; 4];
    let mut decoded: [Vec<(P2, P2)>; 4] = Default::default();
    // Widths from the luma profile are unbiased; mask clusters are fattened by thresholding.
    let width_bias = if edges.luma.is_some() && edges.subpixel_tabs {
        0.0
    } else {
        WIDTH_BIAS_PX
    };
    let mut results: Vec<(Side, SideLines, Vec<SeenTab>, Decoded)> = Vec::new();
    for guess in Side::ALL {
        let Some(sl) = &guessed[guess as usize] else {
            continue;
        };
        let seen = find_tabs(edges, &guessed, sl);
        let dec = decode_tabs(sl, &seen, None, width_bias);
        results.push((guess, *sl, seen, dec));
    }
    // The decoded sides reveal the roll: how far each observed outward normal is turned
    // from where that side's normal points on an unrolled screen. Edges without a decode
    // are then labelled by their normal turned back by that roll, so a gun held sideways
    // or upside down gets all four sides right, not just the ones with readable tabs.
    let (mut sx, mut sy) = (0.0, 0.0);
    for (_, sl, _, dec) in &results {
        if let Some(side) = dec.side {
            let nom = side.outer_line();
            // Angle from nominal (nx, ny) to observed (a, b): rotate nominal by theta.
            let (nx, ny) = (nom[0], nom[1]);
            let (a, b) = (sl.outer.a, sl.outer.b);
            sx += nx * a + ny * b;
            sy += nx * b - ny * a;
        }
    }
    let roll = (sx != 0.0 || sy != 0.0).then(|| sy.atan2(sx));
    for (guess, sl, seen, mut dec) in results {
        let mut side = dec.side.unwrap_or(guess);
        if dec.side.is_none() {
            if let Some(th) = roll {
                let (c, s) = (th.cos(), th.sin());
                let unroll = |v: P2| [c * v[0] + s * v[1], -s * v[0] + c * v[1]];
                // Undo the roll on the observed normal, then classify as if unrolled.
                let n = unroll([sl.outer.a, sl.outer.b]);
                side = side_of(&Line {
                    a: n[0],
                    b: n[1],
                    c: 0.0,
                });
                // The reading direction, likewise unrolled: forward is rightwards on a
                // horizontal side and downwards on a vertical one.
                let d = sl.outer.direction();
                let d = unroll([sl.dir * d[0], sl.dir * d[1]]);
                let forward = if side.is_horizontal() { d[0] } else { d[1] };
                // With side and direction settled, two tabs are enough to place.
                dec = decode_tabs(&sl, &seen, Some((side, forward < 0.0)), width_bias);
            }
        }
        // Two edges claiming one side: keep the one with tab evidence, else the first.
        if sides[side as usize].is_some_and(|_| dec.points.len() <= decoded[side as usize].len()) {
            continue;
        }
        sides[side as usize] = Some(sl);
        decoded[side as usize] = dec.points;
        report.seen[side as usize] = seen;
    }
    for side in Side::ALL {
        report.decoded[side as usize] = decoded[side as usize].len();
        points.extend(decoded[side as usize].iter().copied());
    }
    report.sides = sides;
    let n_tabs = u8::try_from(points.len()).unwrap_or(u8::MAX);
    report.points.clone_from(&points);
    let per_side = report.decoded;
    let geometry = (edges.screen_aspect, edges.border_frac);
    let mut corners = match solve_corners(&sides, &points, &per_side, geometry, mask_w, mask_h) {
        Ok(q) => Some(q),
        Err(why) => {
            report.refused = Some(why);
            None
        }
    };
    // A solve must agree with the tabs it decoded. A four-line solve built on a
    // misassigned edge, or a run matched at the wrong place, lands them far from their
    // screen positions; first retry as a least-squares fit over lines and tabs, then give
    // up rather than pass a wrong answer on.
    let fits = |q: &[P2; 4]| {
        let m = quad_to_quad(q, &SCREEN_PERCENT);
        points.iter().all(|(img, scr)| {
            m.apply(*img)
                .is_some_and(|p| (p[0] - scr[0]).abs() < 2.0 && (p[1] - scr[1]).abs() < 2.0)
        })
    };
    if let Some(q) = &corners {
        if !fits(q) {
            corners = solve_corners_dlt(&sides, &points, &per_side, geometry, mask_w, mask_h)
                .ok()
                .filter(fits);
            if corners.is_none() {
                report.refused = Some("solve disagrees with its own tabs");
            }
        }
    }
    (corners.map(|q| (q, n_tabs)), report)
}

impl Report {
    /// One bit per [`Side`] index for each side with an outer edge line.
    #[must_use]
    pub fn side_mask(&self) -> u8 {
        self.sides
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_some())
            .fold(0, |m, (i, _)| m | (1 << i))
    }
}

fn solve_corners(
    sides: &[Option<SideLines>; 4],
    points: &[(P2, P2)],
    per_side: &[usize; 4],
    geometry: (f64, f64),
    mask_w: usize,
    mask_h: usize,
) -> Result<[P2; 4], &'static str> {
    let n_sides = sides.iter().filter(|s| s.is_some()).count();
    if n_sides < 4 {
        return solve_corners_dlt(sides, points, per_side, geometry, mask_w, mask_h);
    }
    let [top, right, bottom, left] = sides.map(|s| s.map(|s| s.outer));
    let (Some(top), Some(right), Some(bottom), Some(left)) = (top, right, bottom, left) else {
        return Err("no sides");
    };
    let q = [
        top.intersect(&left).ok_or("parallel edges")?,
        top.intersect(&right).ok_or("parallel edges")?,
        bottom.intersect(&right).ok_or("parallel edges")?,
        bottom.intersect(&left).ok_or("parallel edges")?,
    ];
    sane_quad(q, mask_w, mask_h, false)
}

/// Border thickness in screen percent along x and along y, for a screen of the given
/// aspect (width over height) and a border of `frac` of the shorter dimension.
#[must_use]
pub fn screen_thickness(aspect: f64, frac: f64) -> (f64, f64) {
    if aspect >= 1.0 {
        (frac * 100.0 / aspect, frac * 100.0)
    } else {
        (frac * 100.0, frac * 100.0 * aspect)
    }
}

/// Least-squares solve over every line and tab point.
///
/// Points on one line only fix the one-dimensional projectivity along it (three degrees of
/// freedom), so with two sides each needs at least two tabs or the system is rank
/// deficient and the answer is noise. One constraint beyond the minimum is also required,
/// so that the consistency check in [`solve`] has something to check.
fn solve_corners_dlt(
    sides: &[Option<SideLines>; 4],
    points: &[(P2, P2)],
    per_side: &[usize; 4],
    geometry: (f64, f64),
    mask_w: usize,
    mask_h: usize,
) -> Result<[P2; 4], &'static str> {
    let n_sides = sides.iter().filter(|s| s.is_some()).count();
    let mut lines: Vec<(L3, L3)> = Side::ALL
        .iter()
        .filter_map(|&side| {
            sides[side as usize].map(|s| {
                let sl = side.outer_line();
                (line3(&s.outer), [sl[0], sl[1], -sl[2]])
            })
        })
        .collect();
    // A single side, or two sides with a starved one, is short of constraints. The border
    // carries two more drawn lines per side, the inner edge and the tab tips, at one and
    // two thicknesses inward on screen; with a decoded side's tabs fixing the position
    // along it, they supply the direction across it. Precision falls off with distance
    // from the side, but that is where the aim is when only that side is in view.
    let starved = n_sides == 2
        && Side::ALL
            .iter()
            .any(|&sd| sides[sd as usize].is_some() && per_side[sd as usize] < 2);
    // A side far thinner than the thickest one, carrying no tabs, is a false edge (the lit
    // part of a panel mid-refresh ends in a straight line); it must not pin a partial
    // solve. Sides with decoded tabs have proven themselves.
    let thickest = sides
        .iter()
        .flatten()
        .filter_map(|s| s.thickness)
        .fold(0.0_f64, f64::max);
    let false_edge = |sd: Side| {
        per_side[sd as usize] == 0
            && sides[sd as usize].is_some_and(|s| s.thickness.is_some_and(|t| t < 0.5 * thickest))
    };
    if Side::ALL.iter().any(|&sd| false_edge(sd)) {
        let mut kept = *sides;
        for sd in Side::ALL {
            if false_edge(sd) {
                kept[sd as usize] = None;
            }
        }
        return solve_corners_dlt(&kept, points, per_side, geometry, mask_w, mask_h);
    }
    if n_sides == 1 {
        return solve_one_side(sides, points, per_side, geometry, mask_w, mask_h);
    }
    if 2 * n_sides + points.len() < 9 || starved {
        if points.len() < 3 {
            return Err("too few constraints");
        }
        let (tx, ty) = screen_thickness(geometry.0, geometry.1);
        for side in Side::ALL {
            let Some(s) = &sides[side as usize] else {
                continue;
            };
            if per_side[side as usize] < 3 {
                continue;
            }
            let sl = side.outer_line();
            let step = if side.is_horizontal() { ty } else { tx };
            for (img, k) in [(s.inner, 1.0), (s.tips, 2.0)] {
                if let Some(l) = img {
                    // Inward on screen is against the outward normal: c shrinks by k*step.
                    let c = sl[2] - k * step;
                    lines.push((line3(&l), [sl[0], sl[1], -c]));
                }
            }
        }
        if 2 * lines.len() + points.len() < 9 {
            return Err("too few constraints");
        }
    }
    let h = fit_dlt(points, &lines).ok_or("solve failed")?;
    let inv = h.adjugate();
    let mut q = [[0.0; 2]; 4];
    for (c, s) in q.iter_mut().zip(SCREEN_PERCENT.iter()) {
        *c = inv.apply(*s).ok_or("corner at infinity")?;
    }
    // A partial view of a large screen puts the far corners many frame spans away, so
    // the far limit is loosened for partial solves; convexity and the tab consistency
    // check still stand.
    let loose = sides.iter().filter(|s| s.is_some()).count() < 4;
    sane_quad(q, mask_w, mask_h, loose)
}

/// A single visible side. Its tabs fix the map along the edge (including the vanishing
/// point of that direction, since the pitch is constant on screen), and the outer edge
/// fixes the line, which leaves the foreshortening across the side unknown: three parallel
/// lines ten pixels apart cannot tell it. The inner edge's distance in the image against
/// its known screen offset gives the scale across the side, taken as constant over the
/// visible stretch. That is an approximation, exact only without tilt, and it is what
/// makes aiming near a lone edge possible at all; the runtime treats one-side solves as
/// the weakest support.
fn solve_one_side(
    sides: &[Option<SideLines>; 4],
    points: &[(P2, P2)],
    per_side: &[usize; 4],
    geometry: (f64, f64),
    mask_w: usize,
    mask_h: usize,
) -> Result<[P2; 4], &'static str> {
    let side = Side::ALL
        .iter()
        .copied()
        .find(|&sd| sides[sd as usize].is_some())
        .ok_or("no sides")?;
    let sl = sides[side as usize].ok_or("no sides")?;
    if per_side[side as usize] < 3 || points.len() < 3 {
        return Err("too few constraints");
    }
    let (tx, ty) = screen_thickness(geometry.0, geometry.1);
    let t_scr = if side.is_horizontal() { ty } else { tx };
    // Where the inner edge lies in the image: the fitted inner line, else the outer
    // pushed in by the measured thickness.
    let inner = match (sl.inner, sl.thickness) {
        (Some(l), _) => l,
        (None, Some(t)) => sl.outer.offset(-t),
        _ => return Err("no inner edge"),
    };
    // The two tabs farthest apart along the edge, and their feet on the inner edge along
    // the outer normal.
    let (a, b) = points
        .iter()
        .flat_map(|p| points.iter().map(move |q| (p, q)))
        .max_by(|(p, q), (r, s)| {
            let d1 = (p.0[0] - q.0[0]).hypot(p.0[1] - q.0[1]);
            let d2 = (r.0[0] - s.0[0]).hypot(r.0[1] - s.0[1]);
            d1.partial_cmp(&d2).unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or("too few constraints")?;
    let n = [sl.outer.a, sl.outer.b];
    let foot = |p: P2| -> Option<P2> {
        let through = Line::through(p, [p[0] - n[0] * 10.0, p[1] - n[1] * 10.0])?;
        inner.intersect(&through)
    };
    let (qa, qb) = (
        foot(a.0).ok_or("parallel edges")?,
        foot(b.0).ok_or("parallel edges")?,
    );
    // Screen positions: the tabs' points on the outer edge and one thickness inward.
    let inward = |p: P2| -> P2 {
        let l = side.outer_line();
        [p[0] - l[0] * t_scr, p[1] - l[1] * t_scr]
    };
    let src = [a.0, b.0, qb, qa];
    let dst = [a.1, b.1, inward(b.1), inward(a.1)];
    let h = quad_to_quad(&src, &dst);
    let inv = h.adjugate();
    let mut q = [[0.0; 2]; 4];
    for (c, s) in q.iter_mut().zip(SCREEN_PERCENT.iter()) {
        *c = inv.apply(*s).ok_or("corner at infinity")?;
    }
    sane_quad(q, mask_w, mask_h, true)
}

fn sane_quad(
    q: [P2; 4],
    mask_w: usize,
    mask_h: usize,
    loose: bool,
) -> Result<[P2; 4], &'static str> {
    // Sanity: a convex quad with the expected winding and no absurd extrapolation.
    #[allow(clippy::cast_precision_loss)]
    let limit = if loose { 40.0 } else { 8.0 } * (mask_w + mask_h) as f64;
    if q.iter().any(|c| !c[0].is_finite() || !c[1].is_finite()) {
        return Err("corner not finite");
    }
    if q.iter().any(|c| c[0].abs() > limit || c[1].abs() > limit) {
        return Err("corners too far off frame");
    }
    for i in 0..4 {
        if cross(q[i], q[(i + 1) % 4], q[(i + 2) % 4]) <= 0.0 {
            return Err("quad not convex");
        }
        let (a, b) = (q[i], q[(i + 1) % 4]);
        if (a[0] - b[0]).hypot(a[1] - b[1]) < 20.0 {
            return Err("quad too small");
        }
    }
    Ok(q)
}

/// The solve report for a frame, for diagnostics (what `acquire` saw, without the hull
/// fallback).
#[must_use]
pub fn report(luma: &[u8], w: usize, h: usize, p: &AcquireParams) -> Report {
    let mask = decimate_threshold(luma, w, h, p.threshold);
    let (labels, blobs) = label(&mask);
    let lens = Lens::centred(p.lens_k1, w, h);
    let (pooled, _) = pooled_boundary(&labels, mask.w, mask.h, &blobs, p.min_size as usize);
    let edges = edge_segments(&pooled, &mask, &lens, p, Some(luma));
    solve(&edges, mask.w, mask.h).1
}

/// Run the whole acquisition on a full-resolution luma frame.
pub fn acquire(luma: &[u8], w: usize, h: usize, p: &AcquireParams) -> Option<Quad> {
    let mask = decimate_threshold(luma, w, h, p.threshold);
    let (labels, blobs) = label(&mask);
    let lens = Lens::centred(p.lens_k1, w, h);
    let min = p.min_size as usize;
    let (pooled, first) = pooled_boundary(&labels, mask.w, mask.h, &blobs, min);
    if let Some(blob) = first {
        let edges = edge_segments(&pooled, &mask, &lens, p, Some(luma));
        if let (Some((corners, tabs)), report) = solve(&edges, mask.w, mask.h) {
            return Some(Quad {
                corners,
                blob,
                clipped: false,
                from_lines: true,
                tabs,
                sides: report.side_mask(),
            });
        }
    }
    for blob in blobs.iter().take(8) {
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
            lens.undistort(refine_corner(
                luma,
                w,
                h,
                q[i],
                centroid,
                p.threshold,
                p.refine_radius,
            ))
        });
        // A hull quad is never trusted. If the line fit failed on a blob that does not touch
        // the frame edge, the shape is not a four-sided border at all (recorded: a border
        // page that was not yet fullscreen gave the hull a confident, wrong quad for 133
        // frames), and if it does touch the edge the corners are synthesised.
        return Some(Quad {
            corners,
            blob: *blob,
            clipped: true,
            from_lines: false,
            tabs: 0,
            sides: 0,
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

/// Map a point through the same [`Flip`] that is applied to the image.
///
/// The bore offset is measured in the camera's own frame, so once the image is flipped for
/// detection the aim point has to be flipped with it. Leaving it unflipped applies the offset
/// backwards, which shows up as a constant aim bias of twice the bore offset.
pub fn flip_point(p: P2, w: usize, h: usize, flip: Flip) -> P2 {
    // `flip_luma` reverses the pixel order, so index i becomes len-1-i; the point transform
    // has to use the same convention or it lands a pixel out.
    #[allow(clippy::cast_precision_loss)]
    let (fw, fh) = ((w - 1) as f64, (h - 1) as f64);
    match flip {
        Flip::None => p,
        Flip::Horizontal => [fw - p[0], p[1]],
        Flip::Vertical => [p[0], fh - p[1]],
        Flip::Both => [fw - p[0], fh - p[1]],
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
pub(crate) mod tests {
    use super::*;

    /// Draw a hollow quad border given in undistorted coordinates, as a lens with
    /// coefficient `k1` would image it.
    pub(crate) fn synth_through_lens(
        w: usize,
        h: usize,
        outer: &[P2; 4],
        inset: f64,
        k1: f64,
    ) -> Vec<u8> {
        let lens = Lens::centred(k1, w, h);
        let c = [
            (outer[0][0] + outer[1][0] + outer[2][0] + outer[3][0]) / 4.0,
            (outer[0][1] + outer[1][1] + outer[2][1] + outer[3][1]) / 4.0,
        ];
        let inner: [P2; 4] = core::array::from_fn(|i| {
            [
                c[0] + (outer[i][0] - c[0]) * inset,
                c[1] + (outer[i][1] - c[1]) * inset,
            ]
        });
        let inside = |q: &[P2; 4], p: P2| (0..4).all(|i| cross(q[i], q[(i + 1) % 4], p) >= 0.0);
        let mut img = vec![10u8; w * h];
        for y in 0..h {
            for x in 0..w {
                #[allow(clippy::cast_precision_loss)]
                let u = lens.undistort([x as f64, y as f64]);
                if inside(outer, u) && !inside(&inner, u) {
                    img[y * w + x] = 230;
                }
            }
        }
        img
    }

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
        assert!(!q.clipped && q.from_lines);
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

    /// All four corners outside the frame, all four edges crossing it: the line fits must
    /// still recover the corners.
    #[test]
    fn finds_border_with_corners_off_frame() {
        let (w, h) = (640, 480);
        let truth = [[30.0, -30.0], [700.0, 30.0], [600.0, 510.0], [-30.0, 440.0]];
        let img = synth(w, h, &truth, 0.96);
        let q = acquire(&img, w, h, &AcquireParams::default()).expect("border found");
        assert!(!q.clipped && q.from_lines, "{q:?}");
        for (i, (got, want)) in q.corners.iter().zip(truth.iter()).enumerate() {
            let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
            assert!(d <= 4.0, "corner {i}: got {got:?} want {want:?} (d={d:.2})");
        }
    }

    /// Draw the coded border (ring plus tabs, 16:9 screen, 3% thickness) as seen through
    /// `screen_from_image` (undistorted image pixels to screen percent) and a lens.
    pub(crate) fn synth_coded(w: usize, h: usize, screen_from_image: &Mat3, k1: f64) -> Vec<u8> {
        let lens = Lens::centred(k1, w, h);
        let (tx, ty) = (3.0 * 9.0 / 16.0, 3.0);
        let mut img = vec![10u8; w * h];
        for y in 0..h {
            for x in 0..w {
                #[allow(clippy::cast_precision_loss)]
                let u = lens.undistort([x as f64, y as f64]);
                let Some([sx, sy]) = screen_from_image.apply(u) else {
                    continue;
                };
                if !(0.0..=100.0).contains(&sx) || !(0.0..=100.0).contains(&sy) {
                    continue;
                }
                let ring = sx < tx || sx > 100.0 - tx || sy < ty || sy > 100.0 - ty;
                let tab = Side::ALL.iter().any(|&side| {
                    code::tabs(side).iter().any(|t| match side {
                        Side::Top => {
                            (t.start..=t.end).contains(&sx) && (ty..=2.0 * ty).contains(&sy)
                        }
                        Side::Bottom => {
                            (t.start..=t.end).contains(&sx)
                                && (100.0 - 2.0 * ty..=100.0 - ty).contains(&sy)
                        }
                        Side::Left => {
                            (t.start..=t.end).contains(&sy) && (tx..=2.0 * tx).contains(&sx)
                        }
                        Side::Right => {
                            (t.start..=t.end).contains(&sy)
                                && (100.0 - 2.0 * tx..=100.0 - tx).contains(&sx)
                        }
                    })
                });
                if ring || tab {
                    img[y * w + x] = 230;
                }
            }
        }
        img
    }

    fn check_corners(q: &Quad, truth: &[P2; 4], tol: f64) {
        for (i, (got, want)) in q.corners.iter().zip(truth.iter()).enumerate() {
            let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
            assert!(d <= tol, "corner {i}: got {got:?} want {want:?} (d={d:.2})");
        }
    }

    /// Whole coded border in view: four-line solve, and the tabs decode.
    #[test]
    fn coded_border_full_view_reads_tabs() {
        let (w, h) = (640, 480);
        let truth = [[60.0, 60.0], [590.0, 40.0], [600.0, 440.0], [50.0, 430.0]];
        let h_si = quad_to_quad(&truth, &SCREEN_PERCENT);
        let img = synth_coded(w, h, &h_si, 0.0);
        let q = acquire(&img, w, h, &AcquireParams::default()).expect("border found");
        assert!(q.from_lines, "{q:?}");
        check_corners(&q, &truth, 2.5);
        assert!(q.tabs >= 8, "only {} tabs decoded", q.tabs);
    }

    /// Only the top-left corner in view (two sides): the tabs must close the solve.
    #[test]
    fn coded_border_two_sides_solves_from_tabs() {
        let (w, h) = (640, 480);
        let k1 = -0.1;
        // The screen is about twice the frame; its top-left corner sits inside the frame
        // with roughly half of the top and left sides in view.
        let truth = [
            [120.0, 90.0],
            [1100.0, 70.0],
            [1140.0, 760.0],
            [90.0, 800.0],
        ];
        let h_si = quad_to_quad(&truth, &SCREEN_PERCENT);
        let img = synth_coded(w, h, &h_si, k1);
        let p = AcquireParams {
            lens_k1: k1,
            ..Default::default()
        };
        let q = acquire(&img, w, h, &p).expect("border found");
        assert!(q.from_lines && q.tabs >= 4, "{q:?}");
        // The near corner is exact-ish; the far ones are extrapolated a long way.
        let d0 = ((q.corners[0][0] - truth[0][0]).powi(2)
            + (q.corners[0][1] - truth[0][1]).powi(2))
        .sqrt();
        assert!(d0 <= 3.0, "TL {:?} vs {:?}", q.corners[0], truth[0]);
        // What matters is aim near the visible region: a screen point in view maps back well.
        let m = q.to_screen();
        for s in [[10.0, 10.0], [25.0, 5.0], [5.0, 20.0]] {
            let cam = h_si.adjugate().apply(s).expect("finite");
            let got = m.apply(cam).expect("finite");
            assert!(
                (got[0] - s[0]).abs() < 1.0 && (got[1] - s[1]).abs() < 1.0,
                "{s:?} -> {got:?}"
            );
        }
    }

    /// A corner with only two tabs of the vertical side in view: the top's tabs settle
    /// the side and roll, after which two tabs place themselves and the solve closes.
    #[test]
    fn coded_border_corner_with_two_vertical_tabs() {
        let (w, h) = (640, 480);
        // Top-left corner in view; the left side shows about a third of its height.
        let truth = [
            [100.0, 60.0],
            [1200.0, 40.0],
            [1240.0, 900.0],
            [80.0, 930.0],
        ];
        let h_si = quad_to_quad(&truth, &SCREEN_PERCENT);
        let img = synth_coded(w, h, &h_si, 0.0);
        let r = report(&img, w, h, &AcquireParams::default());
        let q = acquire(&img, w, h, &AcquireParams::default()).expect("border found");
        assert!(
            q.from_lines && q.sides & 0b1001 == 0b1001,
            "{q:?}\nrefused {:?} seen {:?} decoded {:?} sides {:?}",
            r.refused,
            r.seen
                .iter()
                .map(|v| v
                    .iter()
                    .map(|t| (t.along.round(), t.width.round(), t.partial))
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            r.decoded,
            r.sides
                .iter()
                .map(|s| s.map(|s| s.thickness))
                .collect::<Vec<_>>()
        );
        let m = q.to_screen();
        for s in [[10.0, 10.0], [30.0, 5.0], [5.0, 20.0]] {
            let cam = h_si.adjugate().apply(s).expect("finite");
            let got = m.apply(cam).expect("finite");
            assert!(
                (got[0] - s[0]).abs() < 1.5 && (got[1] - s[1]).abs() < 1.5,
                "{s:?} -> {got:?}"
            );
        }
    }

    /// Only two sides visible: no line solution, and whatever the hull fallback returns
    /// must not claim to be a trustworthy quad.
    #[test]
    fn two_sides_is_not_a_quad() {
        let (w, h) = (640, 480);
        let truth = [
            [-300.0, -200.0],
            [500.0, -220.0],
            [520.0, 400.0],
            [-320.0, 420.0],
        ];
        let img = synth(w, h, &truth, 0.96);
        let q = acquire(&img, w, h, &AcquireParams::default());
        assert!(q.is_none_or(|q| q.clipped && !q.from_lines), "{q:?}");
    }

    /// A barrel-distorted view of a straight border is recovered once the lens is known.
    #[test]
    fn undistorts_before_fitting() {
        let (w, h) = (640, 480);
        let k1 = -0.08;
        let truth = [[60.0, 40.0], [590.0, 30.0], [600.0, 450.0], [50.0, 440.0]];
        let img = synth_through_lens(w, h, &truth, 0.95, k1);
        let p = AcquireParams {
            lens_k1: k1,
            ..Default::default()
        };
        let q = acquire(&img, w, h, &p).expect("border found");
        assert!(q.from_lines, "{q:?}");
        for (i, (got, want)) in q.corners.iter().zip(truth.iter()).enumerate() {
            let d = ((got[0] - want[0]).powi(2) + (got[1] - want[1]).powi(2)).sqrt();
            assert!(d <= 2.5, "corner {i}: got {got:?} want {want:?} (d={d:.2})");
        }
    }

    #[test]
    fn flip_point_matches_image_flip() {
        let p = [347.9, 219.9];
        assert_eq!(flip_point(p, 640, 480, Flip::None), p);
        let b = flip_point(p, 640, 480, Flip::Both);
        assert!(
            (b[0] - 291.1).abs() < 1e-9 && (b[1] - 259.1).abs() < 1e-9,
            "{b:?}"
        );
        // Flipping twice is the identity (to within floating-point noise).
        let back = flip_point(b, 640, 480, Flip::Both);
        assert!(
            (back[0] - p[0]).abs() < 1e-9 && (back[1] - p[1]).abs() < 1e-9,
            "{back:?}"
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
