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

/// Each side's symbol sequence. Every window of three consecutive symbols, read in either
/// direction, occurs exactly once across all four sides and is not its own reverse, and
/// every window of two occurs once within its side (checked by test). Three adjacent tabs
/// identify the side, the position and the reading direction; once those are known from
/// another edge, two adjacent tabs place themselves on their side. That is what makes the solve independent of how the gun is rolled:
/// an edge's normal only guesses which side it is; the tabs settle it.
pub const SEQUENCES: [&[u8]; 4] = [
    &[0, 0, 1, 1, 2, 0, 3, 1, 0], // top
    &[1, 1, 3, 2, 1, 0],          // right
    &[2, 2, 1, 3, 3, 2, 0, 0, 3], // bottom
    &[0, 3, 2, 2, 0, 1],          // left
];

/// Number of symbols; symbol `s` is a tab `s + 1` units wide.
pub const SYMBOLS: u8 = 4;

/// Centre-to-centre spacing of tabs, in units. The gap after a tab is what is left.
pub const PITCH_UNITS: f64 = 6.0;

/// Tab width unit and end margin for each side, in percent of the side.
#[must_use]
pub fn unit_and_margin(side: Side) -> (f64, f64) {
    if side.is_horizontal() {
        (1.6, 5.0)
    } else {
        (2.5, 6.0)
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
    for (i, &symbol) in SEQUENCES[side as usize].iter().enumerate() {
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

/// Where a run of symbols sits: the side, the index of the run's first symbol in that
/// side's tab list, and whether the run was read against the side's direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub side: Side,
    pub index: usize,
    pub reversed: bool,
}

/// Identify a run of at least three symbols. `None` if it matches nowhere or, which the
/// design rules out for correct reads but a misread can produce, in more than one place.
#[must_use]
pub fn identify(run: &[u8]) -> Option<Placement> {
    if run.len() < 3 {
        return None;
    }
    let rev: Vec<u8> = run.iter().rev().copied().collect();
    let mut found = None;
    for side in Side::ALL {
        let all = tabs(side);
        let syms: Vec<u8> = all.iter().map(|t| t.symbol).collect();
        for (reversed, r) in [(false, run), (true, rev.as_slice())] {
            if r.len() > syms.len() {
                continue;
            }
            for i in 0..=syms.len() - r.len() {
                if syms[i..i + r.len()] == *r {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(Placement {
                        side,
                        index: i,
                        reversed,
                    });
                }
            }
        }
    }
    found
}

/// Where a run of at least two symbols sits on a side whose identity and reading
/// direction are already known. `None` if it occurs nowhere or more than once.
#[must_use]
pub fn locate(side: Side, run: &[u8], reversed: bool) -> Option<usize> {
    if run.len() < 2 {
        return None;
    }
    let syms: Vec<u8> = tabs(side).iter().map(|t| t.symbol).collect();
    let r: Vec<u8> = if reversed {
        run.iter().rev().copied().collect()
    } else {
        run.to_vec()
    };
    if r.len() > syms.len() {
        return None;
    }
    let hits: Vec<usize> = (0..=syms.len() - r.len())
        .filter(|&i| syms[i..i + r.len()] == r[..])
        .collect();
    (hits.len() == 1).then(|| hits[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_symbol_windows_are_unique_within_a_side() {
        for side in Side::ALL {
            let t = tabs(side);
            for i in 0..t.len() - 1 {
                let run = [t[i].symbol, t[i + 1].symbol];
                assert_eq!(locate(side, &run, false), Some(i), "{side:?} at {i}");
                assert_eq!(locate(side, &[run[1], run[0]], true), Some(i));
            }
        }
    }

    #[test]
    fn every_window_is_unique_across_sides_and_directions() {
        let mut seen = std::collections::HashSet::new();
        for seq in SEQUENCES {
            for w in seq.windows(3) {
                let r: Vec<u8> = w.iter().rev().copied().collect();
                assert_ne!(w, r.as_slice(), "palindromic window {w:?}");
                assert!(seen.insert(w.to_vec()), "repeated window {w:?}");
                assert!(seen.insert(r), "window {w:?} is another's reverse");
            }
            assert!(seq.iter().all(|&s| s < SYMBOLS));
        }
    }

    #[test]
    fn tabs_fit_and_do_not_overlap() {
        for side in Side::ALL {
            let t = tabs(side);
            let want = if side.is_horizontal() { 9 } else { 6 };
            assert_eq!(t.len(), want, "{side:?}: {} tabs", t.len());
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
    fn three_symbols_identify_side_position_and_direction() {
        for side in Side::ALL {
            let t = tabs(side);
            for i in 0..t.len() - 2 {
                let run = [t[i].symbol, t[i + 1].symbol, t[i + 2].symbol];
                let fwd = identify(&run).expect("placed");
                assert_eq!(
                    fwd,
                    Placement {
                        side,
                        index: i,
                        reversed: false
                    }
                );
                let back = [run[2], run[1], run[0]];
                let rev = identify(&back).expect("placed");
                assert_eq!(
                    rev,
                    Placement {
                        side,
                        index: i,
                        reversed: true
                    }
                );
            }
        }
        assert!(identify(&[0, 0]).is_none());
    }
}
