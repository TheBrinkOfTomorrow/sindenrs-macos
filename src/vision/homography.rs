//! Projective map between the detected border quad and the screen.
//!
//! This is the same Heckbert square-to-quad construction that the analysis notes verified
//! against the stock driver's behaviour (round-trip error ~1e-13). The aim point is the
//! camera's centre pixel (plus the bore offset) pushed through the quad-to-screen map.

/// A 3x3 matrix in row-major order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mat3(pub [[f64; 3]; 3]);

/// A 2-D point.
pub type P2 = [f64; 2];

impl Mat3 {
    pub const IDENTITY: Self = Self([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    pub fn mul(&self, o: &Self) -> Self {
        let a = &self.0;
        let b = &o.0;
        let mut r = [[0.0; 3]; 3];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
            }
        }
        Self(r)
    }

    /// Adjugate (transpose of cofactors). For a homography this is the inverse up to scale,
    /// which is all a projective map needs.
    pub fn adjugate(&self) -> Self {
        let m = &self.0;
        Self([
            [
                m[1][1] * m[2][2] - m[1][2] * m[2][1],
                m[0][2] * m[2][1] - m[0][1] * m[2][2],
                m[0][1] * m[1][2] - m[0][2] * m[1][1],
            ],
            [
                m[1][2] * m[2][0] - m[1][0] * m[2][2],
                m[0][0] * m[2][2] - m[0][2] * m[2][0],
                m[0][2] * m[1][0] - m[0][0] * m[1][2],
            ],
            [
                m[1][0] * m[2][1] - m[1][1] * m[2][0],
                m[0][1] * m[2][0] - m[0][0] * m[2][1],
                m[0][0] * m[1][1] - m[0][1] * m[1][0],
            ],
        ])
    }

    /// Apply to a point in homogeneous form. Returns `None` if the point maps to infinity.
    pub fn apply(&self, p: P2) -> Option<P2> {
        let m = &self.0;
        let x = m[0][0] * p[0] + m[0][1] * p[1] + m[0][2];
        let y = m[1][0] * p[0] + m[1][1] * p[1] + m[1][2];
        let w = m[2][0] * p[0] + m[2][1] * p[1] + m[2][2];
        if w.abs() < 1e-300 {
            return None;
        }
        Some([x / w, y / w])
    }
}

/// Map the unit square (0,0)-(1,0)-(1,1)-(0,1) onto `q` given as TL, TR, BR, BL.
pub fn square_to_quad(q: &[P2; 4]) -> Mat3 {
    let [x0, y0] = q[0];
    let [x1, y1] = q[1];
    let [x2, y2] = q[2];
    let [x3, y3] = q[3];
    let sx = x0 - x1 + x2 - x3;
    let sy = y0 - y1 + y2 - y3;
    if sx.abs() < 1e-13 && sy.abs() < 1e-13 {
        // Affine case.
        Mat3([
            [x1 - x0, x2 - x1, x0],
            [y1 - y0, y2 - y1, y0],
            [0.0, 0.0, 1.0],
        ])
    } else {
        let dx1 = x1 - x2;
        let dx2 = x3 - x2;
        let dy1 = y1 - y2;
        let dy2 = y3 - y2;
        let den = dx1 * dy2 - dx2 * dy1;
        let g = (sx * dy2 - dx2 * sy) / den;
        let h = (dx1 * sy - sx * dy1) / den;
        Mat3([
            [(x1 - x0) + g * x1, (x3 - x0) + h * x3, x0],
            [(y1 - y0) + g * y1, (y3 - y0) + h * y3, y0],
            [g, h, 1.0],
        ])
    }
}

/// Map quad `src` onto quad `dst` (both TL, TR, BR, BL).
pub fn quad_to_quad(src: &[P2; 4], dst: &[P2; 4]) -> Mat3 {
    square_to_quad(dst).mul(&square_to_quad(src).adjugate())
}

/// Screen corners in percent, TL, TR, BR, BL. The stock driver uses 0..99; we use 0..100 so
/// the result is directly a percentage.
pub const SCREEN_PERCENT: [P2; 4] = [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]];

/// A line `a x + b y = c` as a homogeneous triple `(a, b, -c)`.
pub type L3 = [f64; 3];

/// Eigenvector of the smallest eigenvalue of a symmetric matrix (cyclic Jacobi).
#[allow(clippy::needless_range_loop)] // index-heavy numeric kernel; iterators obscure it
fn smallest_eigenvector(mut a: [[f64; 9]; 9]) -> [f64; 9] {
    let mut v = [[0.0; 9]; 9];
    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    for _ in 0..60 {
        let mut off = 0.0;
        for p in 0..9 {
            for q in p + 1..9 {
                off += a[p][q] * a[p][q];
            }
        }
        if off < 1e-24 {
            break;
        }
        for p in 0..9 {
            for q in p + 1..9 {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let sn = t * c;
                for k in 0..9 {
                    let (akp, akq) = (a[k][p], a[k][q]);
                    a[k][p] = c * akp - sn * akq;
                    a[k][q] = sn * akp + c * akq;
                }
                for k in 0..9 {
                    let (apk, aqk) = (a[p][k], a[q][k]);
                    a[p][k] = c * apk - sn * aqk;
                    a[q][k] = sn * apk + c * aqk;
                }
                for row in &mut v {
                    let (vp, vq) = (row[p], row[q]);
                    row[p] = c * vp - sn * vq;
                    row[q] = sn * vp + c * vq;
                }
            }
        }
    }
    let mut best = 0;
    for i in 1..9 {
        if a[i][i] < a[best][best] {
            best = i;
        }
    }
    let mut out = [0.0; 9];
    for (k, o) in out.iter_mut().enumerate() {
        *o = v[k][best];
    }
    out
}

/// Similarity that moves `pts` to centroid 0 and typical radius 1, applied to a point.
fn normaliser(pts: &[P2]) -> Mat3 {
    #[allow(clippy::cast_precision_loss)]
    let n = pts.len().max(1) as f64;
    let (cx, cy) = pts
        .iter()
        .fold((0.0, 0.0), |(x, y), p| (x + p[0] / n, y + p[1] / n));
    let mut r = pts
        .iter()
        .map(|p| ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt())
        .sum::<f64>()
        / n;
    if r < 1e-9 {
        r = 1.0;
    }
    let s = std::f64::consts::SQRT_2 / r;
    Mat3([[s, 0.0, -s * cx], [0.0, s, -s * cy], [0.0, 0.0, 1.0]])
}

/// Direct linear transform from mixed correspondences: `points` are (source, destination)
/// pairs, `lines` are (source, destination) homogeneous lines. Each point gives two
/// equations and each line two, so eight are needed in total (four lines, or two lines and
/// four points, ...). Returns the map from source to destination, or `None` if degenerate.
#[must_use]
pub fn fit_dlt(points: &[(P2, P2)], lines: &[(L3, L3)]) -> Option<Mat3> {
    if 2 * (points.len() + lines.len()) < 8 {
        return None;
    }
    // Hartley normalisation: condition both sides from the point-like content we have.
    let src_pts: Vec<P2> = points
        .iter()
        .map(|(a, _)| *a)
        .chain(lines.iter().filter_map(|(l, _)| line_anchor(*l)))
        .collect();
    let dst_pts: Vec<P2> = points
        .iter()
        .map(|(_, b)| *b)
        .chain(lines.iter().filter_map(|(_, l)| line_anchor(*l)))
        .collect();
    let ts = normaliser(&src_pts);
    let td = normaliser(&dst_pts);
    // Lines transform by the inverse transpose; for a similarity T, l' = adj(T)^T l.
    let ts_l = ts.adjugate();
    let td_l = td.adjugate();
    let tl = |t: &Mat3, l: L3| -> L3 {
        let m = &t.0;
        let out = [
            m[0][0] * l[0] + m[1][0] * l[1] + m[2][0] * l[2],
            m[0][1] * l[0] + m[1][1] * l[1] + m[2][1] * l[2],
            m[0][2] * l[0] + m[1][2] * l[1] + m[2][2] * l[2],
        ];
        let n = (out[0] * out[0] + out[1] * out[1]).sqrt().max(1e-12);
        [out[0] / n, out[1] / n, out[2] / n]
    };
    let mut rows: Vec<[f64; 9]> = Vec::new();
    for (a, b) in points {
        let (Some(a), Some(b)) = (ts.apply(*a), td.apply(*b)) else {
            return None;
        };
        let (x, y, u, v) = (a[0], a[1], b[0], b[1]);
        rows.push([0.0, 0.0, 0.0, -x, -y, -1.0, v * x, v * y, v]);
        rows.push([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y, -u]);
    }
    for (l, m) in lines {
        // m ∝ H^-T l, i.e. H^T m ∝ l: cross(H^T m, l) = 0.
        let l = tl(&ts_l, *l);
        let m = tl(&td_l, *m);
        // (H^T m)_j = sum_i H_ij m_i ; h index = 3 i + j.
        let coeff = |j: usize| -> [f64; 9] {
            let mut r = [0.0; 9];
            for i in 0..3 {
                r[3 * i + j] = m[i];
            }
            r
        };
        let (c0, c1, c2) = (coeff(0), coeff(1), coeff(2));
        let mut r1 = [0.0; 9];
        let mut r2 = [0.0; 9];
        let mut r3 = [0.0; 9];
        for k in 0..9 {
            r1[k] = c0[k] * l[2] - c2[k] * l[0];
            r2[k] = c1[k] * l[2] - c2[k] * l[1];
            r3[k] = c0[k] * l[1] - c1[k] * l[0];
        }
        rows.push(r1);
        rows.push(r2);
        rows.push(r3);
    }
    let mut ata = [[0.0; 9]; 9];
    for r in &rows {
        for i in 0..9 {
            for j in 0..9 {
                ata[i][j] += r[i] * r[j];
            }
        }
    }
    let h = smallest_eigenvector(ata);
    let hn = Mat3([[h[0], h[1], h[2]], [h[3], h[4], h[5]], [h[6], h[7], h[8]]]);
    let full = td.adjugate().mul(&hn).mul(&ts);
    let m = &full.0;
    // Fix the projective scale on the source centroid, which is always a finite,
    // on-screen point. The origin's weight (m[2][2]) is the usual choice but it is zero
    // whenever the frame's corner lies beyond the screen's horizon, which a rolled view
    // of one side at close range reaches with a perfectly good solution.
    #[allow(clippy::cast_precision_loss)]
    let n = src_pts.len().max(1) as f64;
    let (cx, cy) = src_pts
        .iter()
        .fold((0.0, 0.0), |(x, y), p| (x + p[0] / n, y + p[1] / n));
    let scale = m[2][0] * cx + m[2][1] * cy + m[2][2];
    if !scale.is_finite() || scale.abs() < 1e-12 {
        return None;
    }
    Some(Mat3(core::array::from_fn(|i| {
        core::array::from_fn(|j| m[i][j] / scale)
    })))
}

/// The point on a line closest to the origin, as something to normalise on.
fn line_anchor(l: L3) -> Option<P2> {
    let n = l[0] * l[0] + l[1] * l[1];
    if n < 1e-18 {
        return None;
    }
    Some([-l[0] * l[2] / n, -l[1] * l[2] / n])
}

/// Given the detected border corners in camera pixels (TL, TR, BR, BL) and the aim pixel,
/// return the aim point in screen percent.
pub fn aim_percent(corners: &[P2; 4], aim_pixel: P2) -> Option<P2> {
    quad_to_quad(corners, &SCREEN_PERCENT).apply(aim_pixel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random in [-1, 1).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            #[allow(clippy::cast_precision_loss)]
            let v = (self.0 >> 11) as f64 / (1u64 << 53) as f64;
            v * 2.0 - 1.0
        }
    }

    #[test]
    fn round_trip_random_views() {
        let mut rng = Lcg(7);
        let mut max_err: f64 = 0.0;
        for _ in 0..2000 {
            let base = [[60.0, 40.0], [260.0, 40.0], [260.0, 200.0], [60.0, 200.0]];
            let cam: [P2; 4] = core::array::from_fn(|i| {
                [
                    base[i][0] + 35.0 * rng.next(),
                    base[i][1] + 35.0 * rng.next(),
                ]
            });
            let truth = [(rng.next() + 1.0) * 50.0, (rng.next() + 1.0) * 50.0];
            let fwd = quad_to_quad(&SCREEN_PERCENT, &cam);
            let cam_pt = fwd.apply(truth).expect("finite");
            let got = aim_percent(&cam, cam_pt).expect("finite");
            let err = ((got[0] - truth[0]).powi(2) + (got[1] - truth[1]).powi(2)).sqrt();
            max_err = max_err.max(err);
        }
        assert!(max_err < 1e-9, "max round-trip error {max_err}");
    }

    /// The line through two points as a homogeneous triple.
    fn line3(p: P2, q: P2) -> L3 {
        let a = -(q[1] - p[1]);
        let b = q[0] - p[0];
        [a, b, -(a * p[0] + b * p[1])]
    }

    #[test]
    fn dlt_from_four_lines_matches_quad_solution() {
        let cam = [[95.0, 70.0], [548.0, 52.0], [566.0, 415.0], [78.0, 398.0]];
        let lines: Vec<(L3, L3)> = (0..4)
            .map(|i| {
                let (a, b) = (cam[i], cam[(i + 1) % 4]);
                let (sa, sb) = (SCREEN_PERCENT[i], SCREEN_PERCENT[(i + 1) % 4]);
                (line3(a, b), line3(sa, sb))
            })
            .collect();
        let h = fit_dlt(&[], &lines).expect("solve");
        for (c, s) in cam.iter().zip(SCREEN_PERCENT.iter()) {
            let got = h.apply(*c).expect("finite");
            assert!(
                (got[0] - s[0]).abs() < 1e-6 && (got[1] - s[1]).abs() < 1e-6,
                "{got:?} vs {s:?}"
            );
        }
    }

    #[test]
    fn dlt_from_two_lines_and_four_points_on_them() {
        let cam = [[95.0, 70.0], [548.0, 52.0], [566.0, 415.0], [78.0, 398.0]];
        let fwd = quad_to_quad(&SCREEN_PERCENT, &cam);
        // Top and left edges as lines, plus two known points on each.
        let lines = vec![
            (
                line3(cam[0], cam[1]),
                line3(SCREEN_PERCENT[0], SCREEN_PERCENT[1]),
            ),
            (
                line3(cam[3], cam[0]),
                line3(SCREEN_PERCENT[3], SCREEN_PERCENT[0]),
            ),
        ];
        let screen_pts = [[20.0, 0.0], [55.0, 0.0], [0.0, 30.0], [0.0, 70.0]];
        let points: Vec<(P2, P2)> = screen_pts
            .iter()
            .map(|s| (fwd.apply(*s).expect("finite"), *s))
            .collect();
        let h = fit_dlt(&points, &lines).expect("solve");
        for (c, s) in cam.iter().zip(SCREEN_PERCENT.iter()) {
            let got = h.apply(*c).expect("finite");
            assert!(
                (got[0] - s[0]).abs() < 1e-5 && (got[1] - s[1]).abs() < 1e-5,
                "{got:?} vs {s:?}"
            );
        }
        // Not enough constraints: refused.
        assert!(fit_dlt(&points[..2], &lines).is_none());
    }

    #[test]
    fn affine_case_and_centre() {
        let q = [[10.0, 10.0], [110.0, 10.0], [110.0, 60.0], [10.0, 60.0]];
        let c = aim_percent(&q, [60.0, 35.0]).expect("finite");
        assert!((c[0] - 50.0).abs() < 1e-12 && (c[1] - 50.0).abs() < 1e-12);
        let tl = aim_percent(&q, [10.0, 10.0]).expect("finite");
        assert!(tl[0].abs() < 1e-12 && tl[1].abs() < 1e-12);
    }
}
