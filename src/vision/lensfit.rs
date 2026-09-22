//! Estimate the lens distortion coefficient from recorded frames.
//!
//! The border's edges are straight on the screen, so the right `k1` is the one that makes
//! the undistorted edge points fall on straight lines. The search runs the same boundary
//! extraction and line fitting as live detection: a coarse sweep maximises how many
//! boundary points the fitted lines explain (distortion of several pixels pushes points
//! outside the inlier band), then a golden-section search on the residual of those inliers
//! refines the coefficient.

use super::acquire::{
    decimate_threshold, edge_segments, label, pooled_boundary, AcquireParams, Mask,
};
use super::homography::P2;
use super::lens::Lens;

/// One frame reduced to what the fit needs: pooled boundary points at half resolution.
pub struct FitFrame {
    pub pts: Vec<P2>,
    pub mask: Mask,
    pub w: usize,
    pub h: usize,
}

impl FitFrame {
    /// Prepare a luma frame. `None` if no sizeable blob was found.
    #[must_use]
    pub fn prepare(luma: &[u8], w: usize, h: usize, p: &AcquireParams) -> Option<Self> {
        let mask = decimate_threshold(luma, w, h, p.threshold);
        let (labels, blobs) = label(&mask);
        let (pts, first) = pooled_boundary(&labels, mask.w, mask.h, &blobs, p.min_size as usize);
        first.map(|_| Self { pts, mask, w, h })
    }
}

/// How well one `k1` explains the frames: inliers of the fitted lines, and their residual.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Score {
    pub k1: f64,
    pub inliers: usize,
    /// Root-mean-square distance of inliers to their lines, full-resolution pixels.
    pub rms: f64,
}

#[must_use]
pub fn score(frames: &[FitFrame], k1: f64, p: &AcquireParams) -> Score {
    let mut inliers = 0usize;
    let mut sum_sq = 0.0;
    for f in frames {
        let lens = Lens::centred(k1, f.w, f.h);
        for s in edge_segments(&f.pts, &f.mask, &lens, p, None).segments {
            inliers += s.inliers.len();
            #[allow(clippy::cast_precision_loss)]
            {
                sum_sq += s.rms * s.rms * s.inliers.len() as f64;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let rms = if inliers == 0 {
        f64::INFINITY
    } else {
        (sum_sq / inliers as f64).sqrt()
    };
    Score { k1, inliers, rms }
}

/// Search `k1` in `[lo, hi]`. Returns the best score and the coarse sweep for reporting.
#[must_use]
pub fn fit_k1(frames: &[FitFrame], lo: f64, hi: f64, p: &AcquireParams) -> (Score, Vec<Score>) {
    let steps = 40;
    let sweep: Vec<Score> = (0..=steps)
        .map(|i| score(frames, lo + (hi - lo) * f64::from(i) / f64::from(steps), p))
        .collect();
    // Most inliers wins; ties (the plateau around the optimum) go to the lowest residual.
    let best_i = (0..sweep.len())
        .max_by(|&a, &b| {
            sweep[a].inliers.cmp(&sweep[b].inliers).then(
                sweep[b]
                    .rms
                    .partial_cmp(&sweep[a].rms)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
        .unwrap_or(0);
    let step = (hi - lo) / f64::from(steps);
    let (mut a, mut b) = (sweep[best_i].k1 - step, sweep[best_i].k1 + step);
    // Golden-section on the residual within the plateau.
    let phi = (5.0f64.sqrt() - 1.0) / 2.0;
    let mut c = b - phi * (b - a);
    let mut d = a + phi * (b - a);
    let (mut fc, mut fd) = (score(frames, c, p), score(frames, d, p));
    for _ in 0..24 {
        if fc.rms < fd.rms {
            b = d;
            d = c;
            fd = fc;
            c = b - phi * (b - a);
            fc = score(frames, c, p);
        } else {
            a = c;
            c = d;
            fc = fd;
            d = a + phi * (b - a);
            fd = score(frames, d, p);
        }
    }
    let best = if fc.rms < fd.rms { fc } else { fd };
    (best, sweep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::acquire::tests::synth_through_lens;

    #[test]
    fn recovers_synthetic_k1() {
        let (w, h) = (640, 480);
        let truth_k1 = -0.07;
        let p = AcquireParams::default();
        let quads = [
            [[60.0, 40.0], [590.0, 30.0], [600.0, 450.0], [50.0, 440.0]],
            [[30.0, -30.0], [700.0, 30.0], [600.0, 510.0], [-30.0, 440.0]],
            [
                [120.0, 90.0],
                [520.0, 100.0],
                [510.0, 400.0],
                [130.0, 390.0],
            ],
        ];
        let frames: Vec<FitFrame> = quads
            .iter()
            .map(|q| {
                let img = synth_through_lens(w, h, q, 0.95, truth_k1);
                FitFrame::prepare(&img, w, h, &p).expect("blob")
            })
            .collect();
        let (best, _) = fit_k1(&frames, -0.2, 0.05, &p);
        assert!((best.k1 - truth_k1).abs() < 0.01, "{best:?}");
        assert!(best.rms < 1.0, "{best:?}");
    }
}
