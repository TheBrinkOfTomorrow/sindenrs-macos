//! The self-locating border: tabs on the inner edge that say where along a side you are.
//!
//! A plain white border is self-similar, so a fragment of an edge carries no information
//! about which part of the screen it is; with the corners out of frame the homography is
//! underdetermined. Each side therefore carries a row of inward tabs at a constant pitch
//! whose widths spell a ternary sequence in which every window of three consecutive
//! symbols is unique. Reading two or three adjacent tabs identifies them, and each
//! identified tab is a known screen point on that side's outer edge.
//!
//! The layout is fixed in screen percent and shared by the overlay (drawing) and the
//! detector (decoding), so the two can never disagree. Widths are multiples of a unit that
//! roughly matches the border thickness on a 16:9 screen. The detector gets the unit from
//! the centre-to-centre spacing of neighbouring tabs, which thresholding cannot shift
//! (it fattens bright regions, so widths and gaps are biased but centres are not), and
//! reads each width against it; neither the screen's aspect ratio nor a CRT's geometry
//! enters the decode.

use super::homography::P2;

/// A screen edge. Numbered so it can index arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Side {
    Top = 0,
    Right = 1,
    Bottom = 2,
    Left = 3,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Top, Side::Right, Side::Bottom, Side::Left];

    #[must_use]
    pub fn is_horizontal(self) -> bool {
        matches!(self, Side::Top | Side::Bottom)
    }

    /// Screen point (percent) at coordinate `along` on this side's outer edge.
    #[must_use]
    pub fn outer_point(self, along: f64) -> P2 {
        match self {
            Side::Top => [along, 0.0],
            Side::Right => [100.0, along],
            Side::Bottom => [along, 100.0],
            Side::Left => [0.0, along],
        }
    }

    /// This side's outer edge as a line `a x + b y = c` in screen percent, normal outward.
    #[must_use]
    pub fn outer_line(self) -> [f64; 3] {
        match self {
            Side::Top => [0.0, -1.0, 0.0],
            Side::Right => [1.0, 0.0, 100.0],
            Side::Bottom => [0.0, 1.0, 100.0],
            Side::Left => [-1.0, 0.0, 0.0],
        }
    }
}

/// One tab: the span it covers along its side, in percent of that side, and its symbol.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tab {
    pub side: Side,
    pub start: f64,
    pub end: f64,
    pub symbol: u8,
}

impl Tab {
    #[must_use]
    pub fn centre(&self) -> f64 {
        (self.start + self.end) / 2.0
    }
}

/// Symbol sequence, every window of three unique (checked by test). Sides take a prefix.
pub const SEQUENCE: [u8; 16] = [1, 0, 2, 2, 1, 1, 2, 0, 1, 0, 0, 2, 1, 2, 2, 2];

/// Centre-to-centre spacing of tabs, in units. The gap after a tab is what is left.
pub const PITCH_UNITS: f64 = 5.0;

/// Tab width unit and end margin for each side, in percent of the side.
#[must_use]
pub fn unit_and_margin(side: Side) -> (f64, f64) {
    if side.is_horizontal() {
        (1.7, 5.0)
    } else {
        (3.0, 6.0)
    }
}

/// Symbol `s` has width `s + 1` units.
#[must_use]
pub fn width_units(symbol: u8) -> f64 {
    f64::from(symbol) + 1.0
}

/// The tabs on one side, in order of increasing coordinate.
#[must_use]
pub fn tabs(side: Side) -> Vec<Tab> {
    let (unit, margin) = unit_and_margin(side);
    let mut out = Vec::new();
    let pitch = PITCH_UNITS * unit;
    for (i, &symbol) in SEQUENCE.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let centre = margin + (i as f64 + 0.5) * pitch;
        let w = width_units(symbol) * unit;
        if centre + w / 2.0 > 100.0 - margin {
            break;
        }
        out.push(Tab {
            side,
            start: centre - w / 2.0,
            end: centre + w / 2.0,
            symbol,
        });
    }
    out
}

/// Positions in the side's tab list where the symbol run `seen` occurs contiguously.
#[must_use]
pub fn matches(side: Side, seen: &[u8]) -> Vec<usize> {
    let all = tabs(side);
    if seen.is_empty() || seen.len() > all.len() {
        return Vec::new();
    }
    (0..=all.len() - seen.len())
        .filter(|&i| {
            seen.iter()
                .enumerate()
                .all(|(k, &s)| all[i + k].symbol == s)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_window_of_three_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for w in SEQUENCE.windows(3) {
            assert!(seen.insert(w.to_vec()), "repeated window {w:?}");
        }
    }

    #[test]
    fn tabs_fit_and_do_not_overlap() {
        for side in Side::ALL {
            let t = tabs(side);
            assert!(t.len() >= 5, "{side:?}: only {} tabs", t.len());
            for pair in t.windows(2) {
                assert!(pair[0].end < pair[1].start);
            }
            assert!(t[0].start > 0.0 && t[t.len() - 1].end < 100.0);
        }
    }

    #[test]
    fn pitch_is_constant() {
        let t = tabs(Side::Left);
        let (unit, _) = unit_and_margin(Side::Left);
        for pair in t.windows(2) {
            assert!((pair[1].centre() - pair[0].centre() - PITCH_UNITS * unit).abs() < 1e-9);
        }
    }

    #[test]
    fn three_symbols_match_uniquely() {
        let t = tabs(Side::Top);
        for i in 0..t.len() - 2 {
            let seen = [t[i].symbol, t[i + 1].symbol, t[i + 2].symbol];
            assert_eq!(matches(Side::Top, &seen), vec![i]);
        }
        assert!(matches(Side::Top, &[]).is_empty());
    }
}
