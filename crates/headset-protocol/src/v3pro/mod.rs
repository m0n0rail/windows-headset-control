//! The BlackShark V3 Pro's parameter-access command family.
//!
//! Every identifier in this module was observed on the wire while the vendor
//! software drove our own hardware. Nothing here is guessed, and nothing that
//! was not observed can be encoded — see `READ_ALLOWLIST` / `WRITE_ALLOWLIST`.
//!
//! Frame layout (`docs/device-research.md` carries the evidence):
//!
//! ```text
//! byte  0      report ID (0x02)
//! byte  1      status        0x00 command / 0x02 response or event
//! byte  2      transaction   0x60
//! byte  6      data_size     = 4 + payload length
//! bytes 7-8    class / command id, both 0x00 for this family
//! bytes 9-10   command       byte 9 origin, byte 10 bit 7 = write
//! byte  11     role          0x00 request | 0x01 response | 0x02 event
//! byte  12     payload length
//! bytes 13..   payload
//! byte  62     checksum      XOR of bytes 0..61
//! byte  63     reserved
//! ```

pub mod noise;

use crate::codec::{Codec, Param, ParamFrame, ResultCode, Role};
use crate::error::ProtocolError;
use crate::frame::{CONTROL_REPORT_ID, CONTROL_REPORT_LEN};

/// The V3 Pro's protocol, as the [`Codec`] the device layer dispatches through.
pub struct V3Pro;

/// The one instance device profiles refer to.
pub static V3_PRO: V3Pro = V3Pro;

/// Byte 2 carried this value on every observed frame in both directions.
pub const TRANSACTION_ID: u8 = 0x60;

/// Byte 9 on host-originated frames (both reads and writes).
pub const ORIGIN_HOST: u8 = 0x80;
/// Byte 9 on device-originated event frames.
pub const ORIGIN_DEVICE: u8 = 0x00;

/// Bit 7 of byte 10 distinguishes a write from a read of the same parameter.
pub const WRITE_BIT: u8 = 0x80;

/// Offsets into the 64-byte report.
const OFF_STATUS: usize = 1;
const OFF_TRANSACTION: usize = 2;
const OFF_DATA_SIZE: usize = 6;
const OFF_CLASS: usize = 7;
const OFF_COMMAND_ID: usize = 8;
const OFF_ORIGIN: usize = 9;
const OFF_PARAM: usize = 10;
const OFF_ROLE: usize = 11;
const OFF_PAYLOAD_LEN: usize = 12;
const OFF_PAYLOAD: usize = 13;
const OFF_CHECKSUM: usize = 62;

/// Largest payload that fits between `OFF_PAYLOAD` and the checksum byte.
pub const MAX_PAYLOAD: usize = OFF_CHECKSUM - OFF_PAYLOAD;

/// Status byte on a host-originated command.
const STATUS_COMMAND: u8 = 0x00;

/// Parameters this project may read. Every entry was observed being read by the
/// vendor software during the headset's connect handshake.
///
/// Identifiers whose meaning is established are mapped by [`param_id`]. The
/// rest are reachable but deliberately unnamed: see the Unknown bytes policy in
/// `docs/device-research.md`.
pub const READ_ALLOWLIST: [u8; 17] = [
    0x12, 0x15, 0x16, 0x17, 0x19, 0x20, 0x21, 0x2A, 0x2C, 0x55, 0x5C, 0x5D, 0x5F, 0x60, 0x65, 0x66,
    0x6A,
];

/// Parameters this project may write, as the low 7 bits of the command byte.
/// Every entry was observed being written by the vendor software.
pub const WRITE_ALLOWLIST: [u8; 6] = [0x12, 0x18, 0x19, 0x1E, 0x5C, 0x6A];

/// Parameters observed being written with a two-byte payload. Every other
/// writable parameter was observed carrying exactly one.
///
/// The length is part of what was observed, so it is enforced rather than left
/// to the caller: a one-byte write of `0x12` would leave the device's other
/// byte to chance, and a two-byte write of a level was never seen at all.
pub const PAIR_WRITES: [u8; 1] = [0x12];

/// Parameters that take an index operand on read.
pub const INDEXED_READS: [u8; 3] = [0x15, 0x60, 0x65];

/// Parameters read from the dongle itself rather than proxied to the headset.
/// These use [`ORIGIN_DEVICE`] in byte 9 even for host-originated requests,
/// which is how they were observed.
pub const DONGLE_LOCAL: [u8; 1] = [0x20];

/// Sidetone writes are preceded by `0x18 = 01`, which is what the vendor
/// software was observed doing before every level write. Sending the level
/// alone was never seen on the wire.
pub const SIDETONE_ENABLE: u8 = 0x18;

/// The V3 Pro's identifier for each named parameter. Every one is mapped: the
/// vocabulary in [`Param`] was first established on this device.
pub fn param_id(param: Param) -> u8 {
    match param {
        Param::LinkState => 0x20,
        Param::Battery => 0x21,
        Param::Sidetone => 0x19,
        Param::GameChatBalance => 0x5C,
        Param::MicMute => 0x55,
        Param::SliderFunction => 0x6A,
        Param::NoiseCancellation => 0x12,
    }
}

/// Inclusive value range, where one was established by observing the control
/// clamp at both ends. `None` means the range is not known — the value is
/// surfaced without being validated against an invented bound.
pub fn param_range(param: Param) -> Option<(u8, u8)> {
    match param {
        Param::Sidetone => Some((0, 15)),
        Param::GameChatBalance => Some((0, 20)),
        Param::Battery => Some((0, 100)),
        Param::MicMute => Some((0, 1)),
        // Noise cancellation carries two bytes, neither of which is "the"
        // value this range would bound; `noise::ANC_LEVEL_RANGE` bounds the
        // level alone.
        Param::LinkState | Param::SliderFunction | Param::NoiseCancellation => None,
    }
}

fn role_from_byte(b: u8) -> Option<Role> {
    match b {
        0x00 => Some(Role::Request),
        0x01 => Some(Role::Response),
        0x02 => Some(Role::Event),
        _ => None,
    }
}

fn role_to_byte(role: Role) -> u8 {
    match role {
        Role::Request => 0x00,
        Role::Response => 0x01,
        Role::Event => 0x02,
    }
}

/// The result byte a write's response carries.
///
/// Only `0x00` and `0xFF` have been observed. Anything else is surfaced as
/// [`ResultCode::Unknown`] rather than being assumed to mean success.
pub fn result_code(b: u8) -> ResultCode {
    match b {
        0x00 => ResultCode::Ok,
        0xFF => ResultCode::Refused,
        other => ResultCode::Unknown(other),
    }
}

/// XOR of bytes 0..=61. Verified against every captured report.
pub fn checksum(buf: &[u8]) -> u8 {
    buf.iter().take(OFF_CHECKSUM).fold(0u8, |acc, b| acc ^ b)
}

fn origin_for(param: u8) -> u8 {
    if DONGLE_LOCAL.contains(&param) {
        ORIGIN_DEVICE
    } else {
        ORIGIN_HOST
    }
}

fn encode(
    param_byte: u8,
    origin: u8,
    payload: &[u8],
) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLong {
            max: MAX_PAYLOAD,
            actual: payload.len(),
        });
    }
    let mut buf = [0u8; CONTROL_REPORT_LEN];
    buf[0] = CONTROL_REPORT_ID;
    buf[OFF_STATUS] = STATUS_COMMAND;
    buf[OFF_TRANSACTION] = TRANSACTION_ID;
    buf[OFF_DATA_SIZE] = 4 + payload.len() as u8;
    buf[OFF_CLASS] = 0x00;
    buf[OFF_COMMAND_ID] = 0x00;
    buf[OFF_ORIGIN] = origin;
    buf[OFF_PARAM] = param_byte;
    buf[OFF_ROLE] = role_to_byte(Role::Request);
    buf[OFF_PAYLOAD_LEN] = payload.len() as u8;
    buf[OFF_PAYLOAD..OFF_PAYLOAD + payload.len()].copy_from_slice(payload);
    buf[OFF_CHECKSUM] = checksum(&buf);
    Ok(buf)
}

/// Builds a read request for `param`, rejecting any identifier that was not
/// observed being read.
///
/// `index` is supplied only for the parameters observed to take one; passing it
/// for any other parameter is an error rather than being silently dropped.
pub fn encode_read(
    param: u8,
    index: Option<u8>,
) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    if !READ_ALLOWLIST.contains(&param) {
        return Err(ProtocolError::NotAllowlisted {
            param,
            write: false,
        });
    }
    match (index, INDEXED_READS.contains(&param)) {
        (Some(i), true) => encode(param, origin_for(param), &[i]),
        (None, _) => encode(param, origin_for(param), &[]),
        (Some(_), false) => Err(ProtocolError::UnexpectedIndex { param }),
    }
}

/// Builds a write request for `param`, rejecting any identifier that was not
/// observed being written.
///
/// `param` is the bare parameter id; the write bit is applied here so callers
/// cannot construct a write by hand-setting bit 7 on a read-only identifier.
pub fn encode_write(param: u8, value: u8) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    encode_write_payload(param, &[value])
}

/// The payload length this parameter was observed being written with.
pub fn write_payload_len(param: u8) -> usize {
    if PAIR_WRITES.contains(&param) {
        2
    } else {
        1
    }
}

/// Builds a write request carrying a whole payload, for the parameters that
/// were observed being written with more than a single value byte.
///
/// Rejects a payload of a length never seen for that identifier. The multi-byte
/// parameters are read-modify-write on the device — see [`noise`] — and
/// sending a short payload would silently decide the omitted byte.
pub fn encode_write_payload(
    param: u8,
    payload: &[u8],
) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    if !WRITE_ALLOWLIST.contains(&param) {
        return Err(ProtocolError::NotAllowlisted { param, write: true });
    }
    let expected = write_payload_len(param);
    if payload.len() != expected {
        return Err(ProtocolError::WrongWritePayloadLen {
            param,
            expected,
            actual: payload.len(),
        });
    }
    encode(param | WRITE_BIT, origin_for(param), payload)
}

/// Parses a received 64-byte report.
///
/// Returns `Ok(None)` for a well-formed report that does not belong to the
/// parameter family (non-zero class/command-id bytes). Those are surfaced as
/// opaque rather than decoded, because their structure is unverified.
pub fn parse(raw: &[u8]) -> Result<Option<ParamFrame>, ProtocolError> {
    if raw.len() != CONTROL_REPORT_LEN {
        return Err(ProtocolError::UnexpectedLength {
            expected: CONTROL_REPORT_LEN,
            actual: raw.len(),
        });
    }
    if raw[0] != CONTROL_REPORT_ID {
        return Err(ProtocolError::UnexpectedReportId {
            expected: CONTROL_REPORT_ID,
            actual: raw[0],
        });
    }
    let expected = checksum(raw);
    if raw[OFF_CHECKSUM] != expected {
        return Err(ProtocolError::ChecksumMismatch {
            expected,
            actual: raw[OFF_CHECKSUM],
        });
    }
    if raw[OFF_CLASS] != 0x00 || raw[OFF_COMMAND_ID] != 0x00 {
        return Ok(None);
    }

    let data_size = raw[OFF_DATA_SIZE] as usize;
    if data_size < 4 {
        return Err(ProtocolError::ImplausibleDataSize { data_size });
    }
    let payload_len = data_size - 4;
    if payload_len > MAX_PAYLOAD {
        return Err(ProtocolError::ImplausibleDataSize { data_size });
    }
    // The declared length at byte 12 and the length implied by data_size have
    // agreed on every captured frame. Disagreement means the framing model is
    // wrong for this device; refuse rather than pick one and carry on.
    if raw[OFF_PAYLOAD_LEN] as usize != payload_len {
        return Err(ProtocolError::LengthDisagreement {
            declared: raw[OFF_PAYLOAD_LEN],
            implied: payload_len,
        });
    }
    let Some(role) = role_from_byte(raw[OFF_ROLE]) else {
        return Err(ProtocolError::UnknownRole {
            role: raw[OFF_ROLE],
        });
    };

    Ok(Some(ParamFrame {
        param: raw[OFF_PARAM] & !WRITE_BIT,
        is_write: raw[OFF_PARAM] & WRITE_BIT != 0,
        role,
        payload: raw[OFF_PAYLOAD..OFF_PAYLOAD + payload_len].to_vec(),
    }))
}

impl Codec for V3Pro {
    fn name(&self) -> &'static str {
        "blackshark-v3-pro"
    }

    fn id(&self, param: Param) -> Option<u8> {
        Some(param_id(param))
    }

    fn range(&self, param: Param) -> Option<(u8, u8)> {
        param_range(param)
    }

    fn read_allowlist(&self) -> &'static [u8] {
        &READ_ALLOWLIST
    }

    fn write_allowlist(&self) -> &'static [u8] {
        &WRITE_ALLOWLIST
    }

    fn indexed_reads(&self) -> &'static [u8] {
        &INDEXED_READS
    }

    fn write_payload_len(&self, id: u8) -> usize {
        write_payload_len(id)
    }

    fn write_preamble(&self, param: Param) -> Option<(u8, u8)> {
        match param {
            Param::Sidetone => Some((SIDETONE_ENABLE, 1)),
            _ => None,
        }
    }

    fn encode_read(&self, id: u8, index: Option<u8>) -> Result<Vec<u8>, ProtocolError> {
        encode_read(id, index).map(|b| b.to_vec())
    }

    fn encode_write(&self, id: u8, payload: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        encode_write_payload(id, payload).map(|b| b.to_vec())
    }

    fn parse(&self, raw: &[u8]) -> Result<Vec<ParamFrame>, ProtocolError> {
        // One report carries at most one frame on this device.
        parse(raw).map(|f| f.into_iter().collect())
    }

    fn write_result(&self, ack: &ParamFrame) -> Option<ResultCode> {
        ack.value().map(result_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame 235 of the connect-handshake capture: read battery.
    const CAPTURED_BATTERY_REQUEST: [u8; 8] = [0x02, 0x00, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00];
    /// Frame 237: the response, payload 0x34 = 52 percent.
    fn captured_battery_response() -> [u8; CONTROL_REPORT_LEN] {
        let mut b = [0u8; CONTROL_REPORT_LEN];
        b[..8].copy_from_slice(&[0x02, 0x02, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00]);
        b[8] = 0x00;
        b[9] = 0x80;
        b[10] = 0x21;
        b[11] = 0x01;
        b[12] = 0x01;
        b[13] = 0x34;
        b[62] = checksum(&b);
        b
    }

    #[test]
    fn read_request_matches_the_captured_bytes() {
        let buf = encode_read(param_id(Param::Battery), None).unwrap();
        assert_eq!(&buf[..8], &CAPTURED_BATTERY_REQUEST);
        assert_eq!(buf[9], 0x80);
        assert_eq!(buf[10], 0x21);
        assert_eq!(buf[11], 0x00);
        assert_eq!(buf[12], 0x00);
    }

    #[test]
    fn checksum_matches_a_captured_frame() {
        // Captured sidetone write: 80 99 00 01 00 with checksum 0x7f.
        let mut b = [0u8; CONTROL_REPORT_LEN];
        b[..8].copy_from_slice(&[0x02, 0x00, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00]);
        b[9] = 0x80;
        b[10] = 0x98;
        b[12] = 0x01;
        b[13] = 0x01;
        assert_eq!(checksum(&b), 0x7f);
    }

    #[test]
    fn write_sets_the_write_bit_from_the_bare_id() {
        let buf = encode_write(param_id(Param::Sidetone), 0x0F).unwrap();
        assert_eq!(buf[10], 0x99, "0x19 | 0x80");
        assert_eq!(buf[13], 0x0F);
        assert_eq!(buf[6], 5);
    }

    #[test]
    fn game_chat_write_matches_the_captured_command() {
        let buf = encode_write(param_id(Param::GameChatBalance), 0x14).unwrap();
        assert_eq!(buf[10], 0xDC);
    }

    #[test]
    fn link_state_uses_the_dongle_local_origin() {
        let buf = encode_read(param_id(Param::LinkState), None).unwrap();
        assert_eq!(buf[9], ORIGIN_DEVICE, "0x20 was observed read as 00 20");
    }

    #[test]
    fn unobserved_identifiers_cannot_be_encoded() {
        assert!(matches!(
            encode_read(0x33, None),
            Err(ProtocolError::NotAllowlisted {
                param: 0x33,
                write: false
            })
        ));
        assert!(matches!(
            encode_write(0x33, 1),
            Err(ProtocolError::NotAllowlisted {
                param: 0x33,
                write: true
            })
        ));
    }

    #[test]
    fn a_read_only_parameter_cannot_be_written() {
        // Battery is readable but no write was ever observed.
        assert!(!V3_PRO.is_writable(Param::Battery));
        assert!(matches!(
            encode_write(param_id(Param::Battery), 100),
            Err(ProtocolError::NotAllowlisted { .. })
        ));
    }

    #[test]
    fn indexed_reads_carry_the_index() {
        let buf = encode_read(0x60, Some(3)).unwrap();
        assert_eq!(buf[12], 1);
        assert_eq!(buf[13], 3);
    }

    #[test]
    fn an_index_on_a_non_indexed_parameter_is_rejected() {
        assert!(matches!(
            encode_read(param_id(Param::Battery), Some(3)),
            Err(ProtocolError::UnexpectedIndex { param: 0x21 })
        ));
    }

    #[test]
    fn parses_the_captured_battery_response() {
        let f = parse(&captured_battery_response()).unwrap().unwrap();
        assert_eq!(f.param, param_id(Param::Battery));
        assert_eq!(f.role, Role::Response);
        assert_eq!(f.value(), Some(52));
    }

    #[test]
    fn round_trips_every_writable_parameter() {
        for id in WRITE_ALLOWLIST {
            let payload = vec![7u8; write_payload_len(id)];
            let buf = encode_write_payload(id, &payload).unwrap();
            let f = parse(&buf).unwrap().unwrap();
            assert_eq!(f.param, id);
            assert!(f.is_write);
            assert_eq!(f.payload, payload);
        }
    }

    #[test]
    fn a_two_byte_parameter_cannot_be_written_as_one() {
        // 0x12 was only ever seen written with both bytes. A single-byte write
        // would leave the other one to whatever the device already held.
        assert_eq!(
            encode_write(param_id(Param::NoiseCancellation), 1),
            Err(ProtocolError::WrongWritePayloadLen {
                param: 0x12,
                expected: 2,
                actual: 1
            })
        );
    }

    #[test]
    fn a_one_byte_parameter_cannot_be_written_as_two() {
        assert_eq!(
            encode_write_payload(param_id(Param::Sidetone), &[1, 2]),
            Err(ProtocolError::WrongWritePayloadLen {
                param: 0x19,
                expected: 1,
                actual: 2
            })
        );
    }

    #[test]
    fn the_payload_write_path_honours_the_allowlist_too() {
        assert!(matches!(
            encode_write_payload(0x33, &[1]),
            Err(ProtocolError::NotAllowlisted {
                param: 0x33,
                write: true
            })
        ));
    }

    #[test]
    fn a_corrupted_checksum_is_rejected() {
        let mut b = captured_battery_response();
        b[62] ^= 0xFF;
        assert!(matches!(
            parse(&b),
            Err(ProtocolError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn a_non_parameter_family_frame_is_not_decoded() {
        // command_id 0x84 was observed; its structure is unverified.
        let mut b = [0u8; CONTROL_REPORT_LEN];
        b[0] = CONTROL_REPORT_ID;
        b[2] = TRANSACTION_ID;
        b[6] = 1;
        b[8] = 0x84;
        b[62] = checksum(&b);
        assert_eq!(parse(&b).unwrap(), None);
    }

    #[test]
    fn a_length_disagreement_is_refused_not_guessed() {
        let mut b = captured_battery_response();
        b[12] = 5; // declared 5, data_size implies 1
        b[62] = checksum(&b);
        assert!(matches!(
            parse(&b),
            Err(ProtocolError::LengthDisagreement {
                declared: 5,
                implied: 1
            })
        ));
    }

    #[test]
    fn an_implausible_data_size_is_rejected() {
        for size in [0u8, 1, 2, 3, 200] {
            let mut b = captured_battery_response();
            b[6] = size;
            b[12] = size.saturating_sub(4);
            b[62] = checksum(&b);
            assert!(
                matches!(parse(&b), Err(ProtocolError::ImplausibleDataSize { .. })),
                "data_size {size} should be rejected"
            );
        }
    }

    #[test]
    fn an_unknown_role_is_rejected() {
        let mut b = captured_battery_response();
        b[11] = 0x07;
        b[62] = checksum(&b);
        assert!(matches!(
            parse(&b),
            Err(ProtocolError::UnknownRole { role: 7 })
        ));
    }

    #[test]
    fn result_codes_only_claim_what_was_observed() {
        assert_eq!(result_code(0x00), ResultCode::Ok);
        assert_eq!(result_code(0xFF), ResultCode::Refused);
        assert_eq!(result_code(0x01), ResultCode::Unknown(1));
        assert!(!ResultCode::Unknown(1).is_ok());
    }

    #[test]
    fn the_number_of_observed_but_unnamed_parameters_is_pinned() {
        // The README states this count in prose, and `docs/device-research.md`
        // lists them by identifier. It was wrong for exactly as long as it took
        // to identify 0x12 and not notice. Identifying another parameter should
        // fail here, which is the prompt to update both documents.
        let named = READ_ALLOWLIST
            .iter()
            .filter(|id| Param::ALL.iter().any(|p| param_id(*p) == **id))
            .count();
        let unnamed = READ_ALLOWLIST.len() - named;
        assert_eq!(
            unnamed, 10,
            "the count of unnamed readable parameters changed; update README.md \
             and the 'Reads observed whose meaning is unknown' list in \
             docs/device-research.md to match"
        );
    }

    #[test]
    fn named_parameters_are_all_readable() {
        for p in Param::ALL {
            assert!(
                READ_ALLOWLIST.contains(&param_id(p)),
                "{} is named but not readable",
                p.name()
            );
            assert_eq!(Param::from_name(p.name()), Some(p));
        }
    }

    #[test]
    fn named_parameters_are_writable_only_where_a_write_was_observed() {
        let writable: Vec<&str> = Param::ALL
            .into_iter()
            .filter(|p| V3_PRO.is_writable(*p))
            .map(|p| p.name())
            .collect();
        assert_eq!(
            writable,
            vec![
                "sidetone",
                "game-chat",
                "slider-function",
                "noise-cancellation"
            ]
        );
    }

    #[test]
    fn the_codec_encodes_exactly_what_the_free_functions_do() {
        // The trait is a seam, not a second implementation: going through it
        // must put the same bytes on the wire as the code it wraps.
        let id = param_id(Param::Sidetone);
        assert_eq!(
            V3_PRO.encode_read(id, None).unwrap(),
            encode_read(id, None).unwrap().to_vec()
        );
        assert_eq!(
            V3_PRO.encode_write(id, &[7]).unwrap(),
            encode_write(id, 7).unwrap().to_vec()
        );
        assert_eq!(
            V3_PRO.parse(&captured_battery_response()).unwrap(),
            vec![parse(&captured_battery_response()).unwrap().unwrap()]
        );
    }

    #[test]
    fn the_codec_maps_every_identifier_back_to_its_name() {
        for p in Param::ALL {
            assert_eq!(V3_PRO.param_for(param_id(p)), Some(p));
        }
        assert_eq!(V3_PRO.param_for(0x2C), None, "observed but unnamed");
    }

    #[test]
    fn a_frame_outside_the_parameter_family_parses_to_nothing() {
        let mut b = [0u8; CONTROL_REPORT_LEN];
        b[0] = CONTROL_REPORT_ID;
        b[2] = TRANSACTION_ID;
        b[6] = 1;
        b[8] = 0x84;
        b[62] = checksum(&b);
        assert!(V3_PRO.parse(&b).unwrap().is_empty());
    }

    #[test]
    fn only_sidetone_carries_the_observed_preamble() {
        for p in Param::ALL {
            let expected = (p == Param::Sidetone).then_some((SIDETONE_ENABLE, 1));
            assert_eq!(V3_PRO.write_preamble(p), expected, "{}", p.name());
        }
    }
}
