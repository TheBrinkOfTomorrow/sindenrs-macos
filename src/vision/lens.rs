//! Radial lens distortion, one-parameter division model.
//!
//! The camera's barrel distortion bows the border edges by up to ~10 px across a 640-wide
//! frame, far more than the sub-pixel accuracy the line fits reach, so edge points are
//! undistorted before any straight-line geometry is done. The division model
//! `p_u = c + (p_d - c) / (1 + k1 r^2)` (Fitzgibbon 2001), with `r` the distorted radius
//! in units of half the frame width, fits barrel distortion well with a single parameter and
//! has a cheap forward form. `k1 < 0` is barrel (the Sinden camera), `k1 > 0` pincushion.

use super::homography::P2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lens {
    /// Distortion coefficient; 0 disables correction.
    pub k1: f64,
    /// Distortion centre in full-resolution pixels (the frame centre unless calibrated).
    pub centre: P2,
    /// Radius normalisation: half the frame width in pixels.
    pub scale: f64,
}

impl Lens {
    /// A lens centred on a `w` x `h` frame.
    #[must_use]
    pub fn centred(k1: f64, w: usize, h: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let (fw, fh) = (w as f64, h as f64);
        Self {
            k1,
            centre: [(fw - 1.0) / 2.0, (fh - 1.0) / 2.0],
            scale: fw / 2.0,
        }
    }

    /// No correction.
    #[must_use]
    pub fn none(w: usize, h: usize) -> Self {
        Self::centred(0.0, w, h)
    }

    /// Map a distorted (as-captured) pixel to its undistorted position.
    #[must_use]
    pub fn undistort(&self, p: P2) -> P2 {
        if self.k1 == 0.0 {
            return p;
        }
        let dx = (p[0] - self.centre[0]) / self.scale;
        let dy = (p[1] - self.centre[1]) / self.scale;
        let f = 1.0 + self.k1 * (dx * dx + dy * dy);
        [
            self.centre[0] + dx / f * self.scale,
            self.centre[1] + dy / f * self.scale,
        ]
    }

    /// Map an undistorted position back to where the camera would image it. Newton on the
    /// scalar radius; converges in a handful of steps for realistic `k1`.
    #[must_use]
    pub fn distort(&self, p: P2) -> P2 {
        if self.k1 == 0.0 {
            return p;
        }
        let ux = (p[0] - self.centre[0]) / self.scale;
        let uy = (p[1] - self.centre[1]) / self.scale;
        let ru = (ux * ux + uy * uy).sqrt();
        if ru < 1e-12 {
            return p;
        }
        // Solve rd / (1 + k1 rd^2) = ru for rd.
        let mut rd = ru;
        for _ in 0..20 {
            let f = rd / (1.0 + self.k1 * rd * rd) - ru;
            let d = (1.0 - self.k1 * rd * rd) / (1.0 + self.k1 * rd * rd).powi(2);
            if d.abs() < 1e-12 {
                break;
            }
            let step = f / d;
            rd -= step;
            if step.abs() < 1e-12 {
                break;
            }
        }
        let s = rd / ru;
        [
            self.centre[0] + ux * s * self.scale,
            self.centre[1] + uy * s * self.scale,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_identity() {
        let l = Lens::none(640, 480);
        assert_eq!(l.undistort([12.5, 400.0]), [12.5, 400.0]);
        assert_eq!(l.distort([12.5, 400.0]), [12.5, 400.0]);
    }

    #[test]
    fn round_trip() {
        let l = Lens::centred(-0.08, 640, 480);
        for &p in &[[0.0, 0.0], [639.0, 479.0], [320.0, 10.0], [100.0, 300.0]] {
            let u = l.undistort(p);
            let back = l.distort(u);
            assert!(
                (back[0] - p[0]).abs() < 1e-9 && (back[1] - p[1]).abs() < 1e-9,
                "{p:?} -> {u:?} -> {back:?}"
            );
        }
    }

    #[test]
    fn barrel_moves_corners_outward() {
        let l = Lens::centred(-0.08, 640, 480);
        let u = l.undistort([0.0, 0.0]);
        assert!(u[0] < 0.0 && u[1] < 0.0, "{u:?}");
        // Centre is fixed.
        let c = l.undistort(l.centre);
        assert_eq!(c, l.centre);
    }
}
