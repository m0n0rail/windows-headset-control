//! Parameter `0x12`: the headset's noise-control mode and ANC level.
//!
//! Two payload bytes, established by capturing the vendor software driving our
//! own hardware across three sessions (see `docs/device-research.md`):
//!
//! ```text
//! byte 0   mode    0x00 off | 0x01 ANC | 0x50 ambient
//! byte 1   level   ANC strength, 0x01..=0x04
//! ```
//!
//! The write is whole-struct: the vendor software read `0x12` immediately
//! before every write and re-sent the byte it was not changing. A caller that
//! writes without reading first will clobber the other field, which is why this
//! module has no "set the mode" entry point that does not also carry a level.

use super::encode_write_payload;
use crate::error::ProtocolError;
use crate::frame::CONTROL_REPORT_LEN;

/// The parameter this module speaks for.
pub const NOISE_PARAM: u8 = 0x12;

/// The four ANC levels the vendor UI exposes, all of them observed on the wire.
///
/// Unlike sidetone and game/chat, this range is *not* backed by watching the
/// device clamp at both ends — the vendor UI has exactly four positions, so
/// there was nothing to push past. Values outside it are refused rather than
/// sent, because what the device does with them is unknown.
pub const ANC_LEVEL_RANGE: (u8, u8) = (1, 4);

/// Byte 1 as the vendor software was observed sending it when selecting
/// ambient.
///
/// The one ambient write captured carried `0x01` while the ANC level at the
/// time was `4`, and the device went on reporting `4` afterwards. Byte 1 is
/// therefore not the ambient level — ambient has no level — and this is the
/// constant that was seen rather than a value of ours.
const AMBIENT_BYTE1: u8 = 0x01;

/// Byte 0 of parameter `0x12`.
///
/// Only the three mode bytes below were observed. Anything else is surfaced as
/// [`NoiseMode::Unrecognised`] rather than being assumed to mean something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseMode {
    Off,
    /// Active noise cancellation, at the level in byte 1.
    Anc,
    /// Ambient pass-through. Has no level.
    Ambient,
    Unrecognised(u8),
}

impl NoiseMode {
    pub fn from_byte(b: u8) -> NoiseMode {
        match b {
            0x00 => NoiseMode::Off,
            0x01 => NoiseMode::Anc,
            0x50 => NoiseMode::Ambient,
            other => NoiseMode::Unrecognised(other),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            NoiseMode::Off => 0x00,
            NoiseMode::Anc => 0x01,
            NoiseMode::Ambient => 0x50,
            NoiseMode::Unrecognised(b) => b,
        }
    }

    /// CLI-facing name for the modes with an established meaning.
    pub fn name(self) -> Option<&'static str> {
        match self {
            NoiseMode::Off => Some("off"),
            NoiseMode::Anc => Some("anc"),
            NoiseMode::Ambient => Some("ambient"),
            NoiseMode::Unrecognised(_) => None,
        }
    }
}

/// Both payload bytes of parameter `0x12`, which are only ever read and written
/// together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoiseControl {
    pub mode: NoiseMode,
    /// Byte 1. Retained across mode changes: switching off and back on was
    /// observed leaving it untouched.
    pub anc_level: u8,
}

impl NoiseControl {
    /// Decodes a payload, or `None` if it is not the two bytes this parameter
    /// was observed carrying.
    ///
    /// A lone `0xff` — the refusal an unreachable headset produces — is not a
    /// noise state and lands here as `None`.
    pub fn from_payload(payload: &[u8]) -> Option<NoiseControl> {
        match payload {
            [mode, level] => Some(NoiseControl {
                mode: NoiseMode::from_byte(*mode),
                anc_level: *level,
            }),
            _ => None,
        }
    }

    /// The two bytes to put on the wire, as the vendor software was observed
    /// composing them.
    fn to_payload(self) -> [u8; 2] {
        match self.mode {
            NoiseMode::Ambient => [NoiseMode::Ambient.to_byte(), AMBIENT_BYTE1],
            _ => [self.mode.to_byte(), self.anc_level],
        }
    }

    /// Human-readable state. Always names the ANC level, including in the modes
    /// that do not use it, because it is retained and the user's next switch
    /// back to ANC will land on it.
    pub fn describe(self) -> String {
        match self.mode {
            NoiseMode::Anc => format!("anc level {}", self.anc_level),
            NoiseMode::Off => format!("off (anc level {})", self.anc_level),
            NoiseMode::Ambient => format!("ambient (anc level {})", self.anc_level),
            NoiseMode::Unrecognised(b) => format!(
                "unrecognised mode {b:#04x} (byte 1 = {:#04x})",
                self.anc_level
            ),
        }
    }
}

/// Builds the `0x92` write for a complete noise-control state.
///
/// Refuses any mode byte or level that was not observed, rather than sending it
/// and finding out what the device does.
pub fn encode_noise_write(
    control: NoiseControl,
) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    encode_write_payload(NOISE_PARAM, &noise_write_payload(control)?)
}

/// The two payload bytes [`encode_noise_write`] sends, for callers that encode
/// through a [`crate::Codec`]. Applies the same refusals.
pub fn noise_write_payload(control: NoiseControl) -> Result<[u8; 2], ProtocolError> {
    if let NoiseMode::Unrecognised(b) = control.mode {
        return Err(ProtocolError::UnobservedValue {
            param: NOISE_PARAM,
            byte: 0,
            value: b,
        });
    }
    let (lo, hi) = ANC_LEVEL_RANGE;
    if control.anc_level < lo || control.anc_level > hi {
        return Err(ProtocolError::UnobservedValue {
            param: NOISE_PARAM,
            byte: 1,
            value: control.anc_level,
        });
    }
    Ok(control.to_payload())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v3pro::parse;

    #[test]
    fn parses_the_captured_anc_readback() {
        // Session 3, frame 3: read 0x12 while Synapse showed ANC at level 4.
        let c = NoiseControl::from_payload(&[0x01, 0x04]).unwrap();
        assert_eq!(c.mode, NoiseMode::Anc);
        assert_eq!(c.anc_level, 4);
    }

    #[test]
    fn the_level_survives_being_switched_off() {
        // Session 1: write 00 04, and the following read returned 00 04.
        let c = NoiseControl::from_payload(&[0x00, 0x04]).unwrap();
        assert_eq!(c.mode, NoiseMode::Off);
        assert_eq!(c.anc_level, 4, "byte 1 is retained while off");
    }

    #[test]
    fn parses_the_captured_ambient_readback() {
        // Session 2: 50 01 was written and the device reported 50 04.
        let c = NoiseControl::from_payload(&[0x50, 0x04]).unwrap();
        assert_eq!(c.mode, NoiseMode::Ambient);
        assert_eq!(c.anc_level, 4);
    }

    #[test]
    fn a_mode_byte_never_observed_is_not_given_a_meaning() {
        let c = NoiseControl::from_payload(&[0x02, 0x04]).unwrap();
        assert_eq!(c.mode, NoiseMode::Unrecognised(0x02));
        assert_eq!(c.mode.name(), None);
    }

    #[test]
    fn the_refusal_byte_is_not_a_noise_state() {
        assert_eq!(NoiseControl::from_payload(&[0xFF]), None);
        assert_eq!(NoiseControl::from_payload(&[]), None);
        assert_eq!(NoiseControl::from_payload(&[0x01, 0x04, 0x00]), None);
    }

    #[test]
    fn the_anc_level_write_matches_the_captured_bytes() {
        // Session 3, frame 13: 0x8092 with payload 01 03.
        let buf = encode_noise_write(NoiseControl {
            mode: NoiseMode::Anc,
            anc_level: 3,
        })
        .unwrap();
        assert_eq!(buf[10], 0x92, "0x12 | write bit");
        assert_eq!(buf[6], 6, "data_size = 4 + 2");
        assert_eq!(buf[12], 2, "two payload bytes");
        assert_eq!(&buf[13..15], &[0x01, 0x03]);
    }

    #[test]
    fn switching_off_carries_the_retained_level_as_observed() {
        // Session 1, frame 5: 0x8092 with payload 00 04, level 4 at the time.
        let buf = encode_noise_write(NoiseControl {
            mode: NoiseMode::Off,
            anc_level: 4,
        })
        .unwrap();
        assert_eq!(&buf[13..15], &[0x00, 0x04]);
    }

    #[test]
    fn ambient_sends_the_byte_the_vendor_software_sent() {
        // Session 2, frame 5: 0x8092 with payload 50 01, while the ANC level
        // was 4 and stayed 4. Ambient has no level, so byte 1 is reproduced as
        // observed rather than filled with the retained level.
        let buf = encode_noise_write(NoiseControl {
            mode: NoiseMode::Ambient,
            anc_level: 4,
        })
        .unwrap();
        assert_eq!(&buf[13..15], &[0x50, 0x01]);
    }

    #[test]
    fn a_level_outside_the_observed_four_is_refused() {
        for level in [0u8, 5, 255] {
            assert_eq!(
                encode_noise_write(NoiseControl {
                    mode: NoiseMode::Anc,
                    anc_level: level,
                }),
                Err(ProtocolError::UnobservedValue {
                    param: 0x12,
                    byte: 1,
                    value: level
                }),
                "level {level} was never seen on the wire"
            );
        }
    }

    #[test]
    fn an_unrecognised_mode_is_never_written_back() {
        // Read-modify-write of the level must not carry a mode byte we have no
        // evidence for back onto the wire.
        assert_eq!(
            encode_noise_write(NoiseControl {
                mode: NoiseMode::Unrecognised(0x02),
                anc_level: 3,
            }),
            Err(ProtocolError::UnobservedValue {
                param: 0x12,
                byte: 0,
                value: 0x02
            })
        );
    }

    #[test]
    fn a_written_frame_round_trips_through_the_parser() {
        let sent = NoiseControl {
            mode: NoiseMode::Anc,
            anc_level: 2,
        };
        let buf = encode_noise_write(sent).unwrap();
        let f = parse(&buf).unwrap().unwrap();
        assert_eq!(f.param, NOISE_PARAM);
        assert!(f.is_write);
        assert_eq!(NoiseControl::from_payload(&f.payload), Some(sent));
    }

    #[test]
    fn describe_names_the_retained_level_in_every_mode() {
        let at = |mode| NoiseControl { mode, anc_level: 4 };
        assert_eq!(at(NoiseMode::Anc).describe(), "anc level 4");
        assert_eq!(at(NoiseMode::Off).describe(), "off (anc level 4)");
        assert_eq!(at(NoiseMode::Ambient).describe(), "ambient (anc level 4)");
        assert!(at(NoiseMode::Unrecognised(0x02))
            .describe()
            .contains("unrecognised mode 0x02"));
    }
}
