//! Straight-line extraction from edge points.
//!
//! The border's edges are long straight lines in the (undistorted) image, and a line is
//! determined by any visible stretch of it. Fitting lines instead of finding corners is
//! what makes a partly visible border usable: four edge lines pin the four corners whether
//! or not the corners themselves are inside the frame.

use super::homography::P2;

/// A line `a x + b y = c` with `(a, b)` a unit normal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub a: f64,
    pub b: f64,
    pub c: f64,
}

impl Line {
    /// Signed distance from `p` to the line, positive on the side the normal points to.
    #[must_use]
    pub fn signed_dist(&self, p: P2) -> f64 {
        self.a * p[0] + self.b * p[1] - self.c
    }

    /// The line through two points, or `None` if they coincide.
    #[must_use]
    pub fn through(p: P2, q: P2) -> Option<Self> {
        let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-9 {
            return None;
        }
        let (a, b) = (-dy / len, dx / len);
        Some(Self {
            a,
            b,
            c: a * p[0] + b * p[1],
        })
    }

    /// Flip the normal so that `p` lies on its negative side.
    #[must_use]
    pub fn oriented_away_from(self, p: P2) -> Self {
        if self.signed_dist(p) > 0.0 {
            Self {
                a: -self.a,
                b: -self.b,
                c: -self.c,
            }
        } else {
            self
        }
    }

    /// Translate along the normal by `d`.
    #[must_use]
    pub fn offset(self, d: f64) -> Self {
        Self {
            c: self.c + d,
            ..self
        }
    }

    /// Intersection of two lines, or `None` if (nearly) parallel.
    #[must_use]
    pub fn intersect(&self, o: &Self) -> Option<P2> {
        let det = self.a * o.b - self.b * o.a;
        if det.abs() < 1e-9 {
            return None;
        }
        Some([
            (self.c * o.b - self.b * o.c) / det,
            (self.a * o.c - self.c * o.a) / det,
        ])
    }

    /// Cosine of the angle between the two normals (1 = parallel, 0 = perpendicular).
    #[must_use]
    pub fn cos_angle(&self, o: &Self) -> f64 {
        (self.a * o.a + self.b * o.b).abs()
    }

    /// Total least squares fit (principal axis through the centroid). `None` for fewer than
    /// two distinct points.
    #[must_use]
    pub fn fit(points: &[P2]) -> Option<Self> {
        if points.len() < 2 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let n = points.len() as f64;
        let (mx, my) = points
            .iter()
            .fold((0.0, 0.0), |(x, y), p| (x + p[0] / n, y + p[1] / n));
        let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
        for p in points {
            let (dx, dy) = (p[0] - mx, p[1] - my);
            sxx += dx * dx;
            sxy += dx * dy;
            syy += dy * dy;
        }
        // The normal is the eigenvector of the smallest eigenvalue of the 2x2 scatter matrix.
        let tr = sxx + syy;
        let det = sxx * syy - sxy * sxy;
        let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
        let lam = tr / 2.0 - disc; // smallest eigenvalue
        let (a, b) = if sxy.abs() > 1e-12 {
            (lam - syy, sxy)
        } else if sxx >= syy {
            (0.0, 1.0)
        } else {
            (1.0, 0.0)
        };
        let len = (a * a + b * b).sqrt();
        if len < 1e-12 {
            return None;
        }
        let (a, b) = (a / len, b / len);
        Some(Self {
            a,
            b,
            c: a * mx + b * my,
        })
    }
}

/// A line found by [`extract_lines`], with the points that supported it.
#[derive(Clone, Debug)]
pub struct Segment {
    pub line: Line,
    pub inliers: Vec<P2>,
    /// Root-mean-square distance of the inliers to the refit line.
    pub rms: f64,
}

/// Deterministic pseudo-random source so detection is reproducible frame to frame.
struct Lcg(u64);

impl Lcg {
    fn next_below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        #[allow(clippy::cast_possible_truncation)]
        let r = ((self.0 >> 33) as usize) % n;
        r
    }
}

/// Sequential RANSAC: repeatedly find the line with the most points within `tol`, refit it
/// to its inliers, remove them, and continue while a line has at least `min_inliers`
/// support and fewer than `max_lines` have been found. Returns lines largest first.
#[must_use]
pub fn extract_lines(
    points: &[P2],
    tol: f64,
    min_inliers: usize,
    max_lines: usize,
) -> Vec<Segment> {
    let mut remaining: Vec<P2> = points.to_vec();
    let mut out = Vec::new();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let iters = 64;
    while remaining.len() >= min_inliers && out.len() < max_lines {
        let mut best: Option<(Line, usize)> = None;
        for _ in 0..iters {
            let i = rng.next_below(remaining.len());
            let j = rng.next_below(remaining.len());
            let Some(l) = Line::through(remaining[i], remaining[j]) else {
                continue;
            };
            let n = remaining
                .iter()
                .filter(|p| l.signed_dist(**p).abs() <= tol)
                .count();
            if best.is_none_or(|(_, bn)| n > bn) {
                best = Some((l, n));
            }
        }
        let Some((l, n)) = best else { break };
        if n < min_inliers {
            break;
        }
        // Refit to the inliers, then re-select inliers against the refit line so the final
        // support reflects the least-squares line rather than the two seed points.
        let first: Vec<P2> = remaining
            .iter()
            .copied()
            .filter(|p| l.signed_dist(*p).abs() <= tol)
            .collect();
        let line = Line::fit(&first).unwrap_or(l);
        let (inliers, rest): (Vec<P2>, Vec<P2>) = remaining
            .iter()
            .partition(|p| line.signed_dist(**p).abs() <= tol);
        if inliers.len() < min_inliers {
            break;
        }
        let line = Line::fit(&inliers).unwrap_or(line);
        #[allow(clippy::cast_precision_loss)]
        let rms = (inliers
            .iter()
            .map(|p| line.signed_dist(*p).powi(2))
            .sum::<f64>()
            / inliers.len() as f64)
            .sqrt();
        out.push(Segment { line, inliers, rms });
        remaining = rest;
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.inliers.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_recovers_axis_aligned_and_diagonal() {
        let horiz: Vec<P2> = (0..20).map(|i| [f64::from(i), 5.0]).collect();
        let l = Line::fit(&horiz).expect("fit");
        assert!(
            l.a.abs() < 1e-9 && (l.b.abs() - 1.0).abs() < 1e-9 && (l.c.abs() - 5.0).abs() < 1e-9
        );
        let diag: Vec<P2> = (0..20)
            .map(|i| [f64::from(i), f64::from(i) * 2.0 + 1.0])
            .collect();
        let l = Line::fit(&diag).expect("fit");
        for p in &diag {
            assert!(l.signed_dist(*p).abs() < 1e-9);
        }
    }

    #[test]
    fn intersection() {
        let h = Line::through([0.0, 3.0], [10.0, 3.0]).expect("line");
        let v = Line::through([7.0, 0.0], [7.0, 10.0]).expect("line");
        let p = h.intersect(&v).expect("meet");
        assert!((p[0] - 7.0).abs() < 1e-9 && (p[1] - 3.0).abs() < 1e-9);
        assert!(h.intersect(&h.offset(2.0)).is_none());
    }

    #[test]
    fn extracts_rectangle_sides_with_noise() {
        // Four sides of a rectangle plus scattered outliers.
        let mut pts: Vec<P2> = Vec::new();
        for i in 0..100 {
            let t = f64::from(i);
            pts.push([t, 0.3]);
            pts.push([t + 0.4, 60.0]);
            if i < 60 {
                pts.push([-0.2, t]);
                pts.push([99.0, t + 0.1]);
            }
        }
        let mut rng = Lcg(3);
        for _ in 0..40 {
            #[allow(clippy::cast_precision_loss)]
            pts.push([
                rng.next_below(1000) as f64 / 10.0,
                rng.next_below(600) as f64 / 10.0,
            ]);
        }
        let segs = extract_lines(&pts, 1.0, 20, 6);
        assert!(segs.len() >= 4, "found {}", segs.len());
        let mut seen = [false; 4];
        for s in &segs[..4] {
            let l = s.line;
            if l.b.abs() > 0.99 && (l.c.abs() - 0.3).abs() < 0.5 {
                seen[0] = true;
            } else if l.b.abs() > 0.99 && (l.c.abs() - 60.0).abs() < 0.5 {
                seen[1] = true;
            } else if l.a.abs() > 0.99 && l.c.abs() < 0.5 {
                seen[2] = true;
            } else if l.a.abs() > 0.99 && (l.c.abs() - 99.0).abs() < 0.5 {
                seen[3] = true;
            }
            assert!(s.rms < 0.6, "rms {}", s.rms);
        }
        assert_eq!(seen, [true; 4], "{segs:?}");
    }
}
