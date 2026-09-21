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

    #[test]
    fn affine_case_and_centre() {
        let q = [[10.0, 10.0], [110.0, 10.0], [110.0, 60.0], [10.0, 60.0]];
        let c = aim_percent(&q, [60.0, 35.0]).expect("finite");
        assert!((c[0] - 50.0).abs() < 1e-12 && (c[1] - 50.0).abs() < 1e-12);
        let tl = aim_percent(&q, [10.0, 10.0]).expect("finite");
        assert!(tl[0].abs() < 1e-12 && tl[1].abs() < 1e-12);
    }
}
