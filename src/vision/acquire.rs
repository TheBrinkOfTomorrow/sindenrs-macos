//! Border acquisition: find the bright quadrilateral (the screen border) in a luma frame.
//!
//! Threshold and 2x2 decimate, label connected components, take each large blob's boundary,
//! undistort it, fit straight lines to it and intersect the outermost line on each side to
//! get the corners. Because a line is pinned by any visible stretch of it, this works when
//! the corners are outside the frame as long as all four edges cross it. When fewer than
//! four sides are visible it falls back to the convex-hull quad of the blob, which is only
//! honest if the blob is not clipped; a clipped hull quad is flagged as such.

use super::homography::{quad_to_quad, Mat3, P2, SCREEN_PERCENT};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

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

/// Fit straight lines to pooled half-resolution boundary points. Points on the frame edge
/// are discarded (they are where the border leaves the frame, not an edge of it); the rest
/// are pushed to full resolution, undistorted, and handed to the line extractor.
#[must_use]
pub fn edge_segments(
    pts: &[P2],
    mask_w: usize,
    mask_h: usize,
    lens: &Lens,
    p: &AcquireParams,
) -> Vec<Segment> {
    #[allow(clippy::cast_precision_loss)]
    let (xe, ye) = ((mask_w - 1) as f64, (mask_h - 1) as f64);
    let full: Vec<P2> = pts
        .iter()
        .filter(|q| q[0] > 0.0 && q[1] > 0.0 && q[0] < xe && q[1] < ye)
        .map(|q| lens.undistort([q[0] * 2.0 + 0.5, q[1] * 2.0 + 0.5]))
        .collect();
    if full.len() < 4 * p.line_min_points {
        return Vec::new();
    }
    extract_lines(&full, p.line_tol, p.line_min_points, 8)
}

/// Corners from fitted edge lines: the outermost line on each of the four sides,
/// intersected. Returns `None` unless all four sides were seen and the result is a convex
/// quad of plausible size.
#[must_use]
pub fn quad_from_segments(segs: &[Segment], mask_w: usize, mask_h: usize) -> Option<[P2; 4]> {
    let all: usize = segs.iter().map(|s| s.inliers.len()).sum();
    if all == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = all as f64;
    let centroid = segs
        .iter()
        .flat_map(|s| s.inliers.iter())
        .fold([0.0, 0.0], |a, q| [a[0] + q[0] / n, a[1] + q[1] / n]);
    // Outermost line per side: the largest distance from the centroid along its outward
    // normal. The outer edge of a boundary pixel is one full-resolution pixel beyond its
    // centre, so push each line out by that much.
    let mut best: [Option<(f64, Line)>; 4] = [None; 4];
    for s in segs {
        let l = s.line.oriented_away_from(centroid).offset(1.0);
        let d = -l.signed_dist(centroid);
        let i = side_of(&l) as usize;
        if best[i].is_none_or(|(bd, _)| d > bd) {
            best[i] = Some((d, l));
        }
    }
    let [top, right, bottom, left] = best.map(|b| b.map(|(_, l)| l));
    let (top, right, bottom, left) = (top?, right?, bottom?, left?);
    let q = [
        top.intersect(&left)?,
        top.intersect(&right)?,
        bottom.intersect(&right)?,
        bottom.intersect(&left)?,
    ];
    // Sanity: a convex quad with the expected winding and no absurd extrapolation.
    #[allow(clippy::cast_precision_loss)]
    let limit = 8.0 * (mask_w + mask_h) as f64;
    if q.iter()
        .any(|c| !c[0].is_finite() || !c[1].is_finite() || c[0].abs() > limit || c[1].abs() > limit)
    {
        return None;
    }
    for i in 0..4 {
        if cross(q[i], q[(i + 1) % 4], q[(i + 2) % 4]) <= 0.0 {
            return None;
        }
    }
    Some(q)
}

/// Run the whole acquisition on a full-resolution luma frame.
pub fn acquire(luma: &[u8], w: usize, h: usize, p: &AcquireParams) -> Option<Quad> {
    let mask = decimate_threshold(luma, w, h, p.threshold);
    let (labels, blobs) = label(&mask);
    let lens = Lens::centred(p.lens_k1, w, h);
    let min = p.min_size as usize;
    let (pooled, first) = pooled_boundary(&labels, mask.w, mask.h, &blobs, min);
    if let Some(blob) = first {
        let segs = edge_segments(&pooled, mask.w, mask.h, &lens, p);
        if let Some(corners) = quad_from_segments(&segs, mask.w, mask.h) {
            return Some(Quad {
                corners,
                blob,
                clipped: false,
                from_lines: true,
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
