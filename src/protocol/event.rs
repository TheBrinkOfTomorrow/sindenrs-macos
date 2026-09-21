//! Unsolicited gun-to-host bytes, emitted while streaming.

/// An event decoded from the gun's event stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Byte 200: the gun's button combo entered calibration mode.
    EnteredCalibration,
    /// Byte 201: calibration set (the gun also leaves calibration mode).
    CalibrationSet,
    /// Byte 202: left calibration mode without setting.
    ExitedCalibration,
    /// Byte 254 followed by three bytes: button state 1, button state 2, and one byte the
    /// stock driver discards (kept here as `extra`).
    Buttons { state1: u8, state2: u8, extra: u8 },
    /// Byte 120: the stock driver treats this as "trigger enabled" in low-resource mode.
    TriggerEnabled,
    /// Anything else. Logged so the real cadence can be characterised on hardware.
    Unknown(u8),
}

impl Event {
    /// Trigger is bit 0 of button state 1.
    pub fn trigger_held(self) -> Option<bool> {
        match self {
            Self::Buttons { state1, .. } => Some(state1 & 1 != 0),
            _ => None,
        }
    }
}

/// Incremental parser for the event byte stream. Handles a `254` header arriving before
/// its three payload bytes.
#[derive(Debug, Default)]
pub struct EventParser {
    pending: Vec<u8>,
}

impl EventParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            let b = self.pending[i];
            let ev = match b {
                200 => Event::EnteredCalibration,
                201 => Event::CalibrationSet,
                202 => Event::ExitedCalibration,
                120 => Event::TriggerEnabled,
                254 => {
                    if self.pending.len() < i + 4 {
                        break; // wait for the rest of the button report
                    }
                    let ev = Event::Buttons {
                        state1: self.pending[i + 1],
                        state2: self.pending[i + 2],
                        extra: self.pending[i + 3],
                    };
                    i += 3;
                    ev
                }
                other => Event::Unknown(other),
            };
            out.push(ev);
            i += 1;
        }
        self.pending.drain(..i);
        out
    }

    /// Bytes held back waiting for a complete button report.
    pub fn pending(&self) -> &[u8] {
        &self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_byte_events() {
        let mut p = EventParser::new();
        assert_eq!(
            p.feed(&[200, 202, 7]),
            vec![
                Event::EnteredCalibration,
                Event::ExitedCalibration,
                Event::Unknown(7)
            ]
        );
        assert!(p.pending().is_empty());
    }

    #[test]
    fn parses_split_button_report() {
        let mut p = EventParser::new();
        assert_eq!(p.feed(&[254, 1]), vec![]);
        assert_eq!(p.pending(), &[254, 1]);
        assert_eq!(
            p.feed(&[2, 3, 201]),
            vec![
                Event::Buttons {
                    state1: 1,
                    state2: 2,
                    extra: 3
                },
                Event::CalibrationSet
            ]
        );
        assert!(p.pending().is_empty());
        assert_eq!(
            Event::Buttons {
                state1: 1,
                state2: 0,
                extra: 0
            }
            .trigger_held(),
            Some(true)
        );
        assert_eq!(Event::CalibrationSet.trigger_held(), None);
    }
}
