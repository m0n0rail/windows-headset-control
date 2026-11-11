//! The BlackShark V2 Pro's parameter-access command family.
//!
//! Every identifier here was observed on the wire while Razer Synapse drove the
//! contributor's own hardware. `docs/device-research-blackshark-v2-pro.md`
//! records the evidence for every byte, including the session each fact came
//! from. Nothing is carried over from the V3 Pro: where an identifier happens
//! to match, it was confirmed on this device by a session of its own.
//!
//! Host → device (interrupt OUT, 64 bytes):
//!
//! ```text
//! [0]      02        report id
//! [1]      80        constant in every host frame
//! [2]      LEN       byte count from [5]; = BODYLEN + 5
//! [5..6]   50 41     constant in every host frame
//! [7]      08        parameter-access family
//! [9]      BODYLEN   byte count from [10]; = 3 + payload length
//! [10]     CMD       parameter id; bit 7 set = write
//! [12]     N         payload length
//! [13..]   payload, then zeros. No checksum.
//! ```
//!
//! Device → host: `02 LEN` followed by one or more concatenated messages,
//! `LEN` bytes in all. Bytes after them are left over from an earlier report
//! and are not data. Each message is `50 49 FAMILY SEQ ?? ?? ?? ?? L 00` and
//! `L` body bytes; a family `08` body is `CMD ROLE N value…`.

use crate::codec::{Codec, Param, ParamFrame, ResultCode, Role};
use crate::error::ProtocolError;
use crate::frame::{CONTROL_REPORT_ID, CONTROL_REPORT_LEN};

/// The V2 Pro's protocol, as the [`Codec`] the device layer dispatches through.
pub struct V2Pro;

/// The one instance device profiles refer to.
pub static V2_PRO: V2Pro = V2Pro;

/// Byte 1 of every host frame.
const HOST_BYTE1: u8 = 0x80;
/// Bytes 5-6 of every host frame.
const HOST_MARKER: [u8; 2] = [0x50, 0x41];
/// The first two bytes of every device message.
const DEVICE_MARKER: [u8; 2] = [0x50, 0x49];
/// The parameter-access family, in both directions.
const FAMILY_PARAM: u8 = 0x08;

/// Bit 7 of the command byte distinguishes a write from a read.
pub const WRITE_BIT: u8 = 0x80;

const OFF_LEN: usize = 2;
const OFF_MARKER: usize = 5;
const OFF_FAMILY: usize = 7;
const OFF_BODY_LEN: usize = 9;
const OFF_CMD: usize = 10;
const OFF_PAYLOAD_LEN: usize = 12;
const OFF_PAYLOAD: usize = 13;

/// Fixed part of a device message: marker, family, sequence, four undecoded
/// bytes, body length, and a zero.
const MESSAGE_HEADER: usize = 10;

/// Largest payload that fits in one host report.
pub const MAX_PAYLOAD: usize = CONTROL_REPORT_LEN - OFF_PAYLOAD;

/// Identifiers this project may read: every one observed being read by
/// Synapse.
///
/// `0x00` was observed too, and is deliberately left out. It returns the
/// device serial, which this project has no use for and redacts everywhere
/// else, so there is no reason to be able to fetch it.
pub const READ_ALLOWLIST: [u8; 10] = [0x01, 0x02, 0x13, 0x1D, 0x1E, 0x20, 0x21, 0x27, 0x2A, 0x2C];

/// Identifiers this project may write, as bare ids. Every one was observed
/// being written by Synapse.
pub const WRITE_ALLOWLIST: [u8; 9] = [0x13, 0x15, 0x16, 0x18, 0x19, 0x1D, 0x1E, 0x27, 0x2C];

/// Identifiers seen only as unprompted device events. Named, but never sent.
pub const EVENT_ONLY: [u8; 1] = [0x55];

/// The ten-byte custom EQ curve. Every other write carried exactly one byte.
pub const EQ_BANDS: u8 = 0x15;

/// Mic monitoring on/off. Synapse wrote `01` here before every level write.
pub const SIDETONE_ENABLE: u8 = 0x18;

/// The power-saving timeouts Synapse's slider produced, in minutes. `0` is off.
pub const POWER_SAVING_MINUTES: (u8, u8) = (15, 60);

/// Mic EQ modes observed: default, mic boost, broadcast, conference, custom.
pub const MIC_EQ_MODES: [u8; 5] = [0x00, 0x01, 0x02, 0x03, 0xFF];

/// The V2 Pro's identifier for a named parameter, where one was established.
pub fn param_id(param: Param) -> Option<u8> {
    match param {
        Param::LinkState => Some(0x20),
        Param::Battery => Some(0x21),
        Param::Sidetone => Some(0x19),
        Param::MicMute => Some(0x55),
        Param::PowerSavingTimeout => Some(0x2C),
        Param::DoNotDisturb => Some(0x27),
        Param::EqPreset => Some(0x13),
        Param::EqBank => Some(0x1D),
        Param::MicEqMode => Some(0x16),
        // Not established on this device: Synapse shows no such control, and
        // nothing on the wire carried one.
        Param::GameChatBalance | Param::SliderFunction | Param::NoiseCancellation => None,
    }
}

/// Inclusive range for the parameters whose values form one. A parameter with
/// a gap or a set of discrete values is bounded by [`Codec::accepts`] instead.
pub fn param_range(param: Param) -> Option<(u8, u8)> {
    match param {
        // Synapse's slider swept 0-100 onto 0x00-0x0F, one step at a time.
        Param::Sidetone => Some((0, 15)),
        Param::Battery => Some((0, 100)),
        Param::MicMute | Param::DoNotDisturb => Some((0, 1)),
        Param::PowerSavingTimeout => Some((0, POWER_SAVING_MINUTES.1)),
        _ => None,
    }
}

/// The write acknowledgement's status byte.
///
/// `00` was the reply to nearly every write. `01` came back from every `0x16`
/// write and from `0x13` writes that switched EQ bank, and in each case the
/// setting took effect, so for those two it is success. Anywhere else `01` was
/// never seen, and no refusal byte has been observed at all: both are surfaced
/// as unknown rather than guessed at.
pub fn result_code(param: u8, status: u8) -> ResultCode {
    match (param, status) {
        (_, 0x00) => ResultCode::Ok,
        (0x16 | 0x13, 0x01) => ResultCode::Ok,
        (_, other) => ResultCode::Unknown(other),
    }
}

fn role_from_byte(b: u8) -> Option<Role> {
    match b {
        0x01 => Some(Role::Response),
        0x02 => Some(Role::Event),
        _ => None,
    }
}

fn encode(cmd: u8, payload: &[u8]) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLong {
            max: MAX_PAYLOAD,
            actual: payload.len(),
        });
    }
    let body_len = 3 + payload.len() as u8;
    let mut buf = [0u8; CONTROL_REPORT_LEN];
    buf[0] = CONTROL_REPORT_ID;
    buf[1] = HOST_BYTE1;
    buf[OFF_LEN] = body_len + 5;
    buf[OFF_MARKER..OFF_MARKER + 2].copy_from_slice(&HOST_MARKER);
    buf[OFF_FAMILY] = FAMILY_PARAM;
    buf[OFF_BODY_LEN] = body_len;
    buf[OFF_CMD] = cmd;
    buf[OFF_PAYLOAD_LEN] = payload.len() as u8;
    buf[OFF_PAYLOAD..OFF_PAYLOAD + payload.len()].copy_from_slice(payload);
    Ok(buf)
}

/// Builds a read request, refusing an identifier never observed being read.
/// No V2 Pro read was observed taking an index.
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
    if index.is_some() {
        return Err(ProtocolError::UnexpectedIndex { param });
    }
    encode(param, &[])
}

/// The payload length a write of `param` was observed carrying.
pub fn write_payload_len(param: u8) -> usize {
    if param == EQ_BANDS {
        10
    } else {
        1
    }
}

/// Builds a write request from the bare id, refusing an identifier never
/// observed being written or a payload of a length never seen for it.
pub fn encode_write(param: u8, payload: &[u8]) -> Result<[u8; CONTROL_REPORT_LEN], ProtocolError> {
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
    encode(param | WRITE_BIT, payload)
}

/// Decodes every parameter-access message in one input report.
///
/// Messages of other families are skipped: they were observed, but their
/// structure is not decoded. A report whose lengths do not add up is refused
/// rather than partly trusted, because a wrong length would shift every byte
/// after it.
pub fn parse(raw: &[u8]) -> Result<Vec<ParamFrame>, ProtocolError> {
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
    let total = raw[1] as usize;
    let end = 2 + total;
    if end > CONTROL_REPORT_LEN {
        return Err(ProtocolError::ImplausibleDataSize { data_size: total });
    }

    let mut frames = Vec::new();
    let mut at = 2;
    while at < end {
        if at + MESSAGE_HEADER > end {
            return Err(ProtocolError::MalformedMessage {
                offset: at,
                why: "message header runs past the declared length",
            });
        }
        if raw[at..at + 2] != DEVICE_MARKER {
            return Err(ProtocolError::MalformedMessage {
                offset: at,
                why: "message does not start with 50 49",
            });
        }
        let body_len = raw[at + 8] as usize;
        let body_start = at + MESSAGE_HEADER;
        let body_end = body_start + body_len;
        if body_end > end {
            return Err(ProtocolError::MalformedMessage {
                offset: at,
                why: "message body runs past the declared length",
            });
        }
        if raw[at + 2] == FAMILY_PARAM {
            frames.push(parse_param_body(&raw[body_start..body_end], at)?);
        }
        at = body_end;
    }
    Ok(frames)
}

fn parse_param_body(body: &[u8], offset: usize) -> Result<ParamFrame, ProtocolError> {
    let [cmd, role, n, value @ ..] = body else {
        return Err(ProtocolError::MalformedMessage {
            offset,
            why: "parameter message shorter than its three-byte head",
        });
    };
    // The body length and the payload length have agreed in every captured
    // message. Disagreement means the framing model is wrong; refuse rather
    // than pick one.
    if *n as usize != value.len() {
        return Err(ProtocolError::LengthDisagreement {
            declared: *n,
            implied: value.len(),
        });
    }
    let Some(role) = role_from_byte(*role) else {
        return Err(ProtocolError::UnknownRole { role: *role });
    };
    Ok(ParamFrame {
        param: cmd & !WRITE_BIT,
        is_write: cmd & WRITE_BIT != 0,
        role,
        payload: value.to_vec(),
    })
}

impl Codec for V2Pro {
    fn name(&self) -> &'static str {
        "blackshark-v2-pro"
    }

    fn id(&self, param: Param) -> Option<u8> {
        param_id(param)
    }

    fn range(&self, param: Param) -> Option<(u8, u8)> {
        param_range(param)
    }

    fn accepts(&self, param: Param, value: u8) -> bool {
        match param {
            // Off, or a timeout the slider produced; 1-14 was never sent.
            Param::PowerSavingTimeout => {
                let (lo, hi) = POWER_SAVING_MINUTES;
                value == 0 || (lo..=hi).contains(&value)
            }
            Param::MicEqMode => MIC_EQ_MODES.contains(&value),
            _ => self
                .range(param)
                .is_none_or(|(lo, hi)| (lo..=hi).contains(&value)),
        }
    }

    fn standalone_write(&self, param: Param) -> bool {
        // An EQ change was only ever observed as a sequence -- read 0x1e,
        // write 0x93, write 0x9d, and for custom and esports a curve on 0x95.
        // Writing the preset or the bank alone was never seen.
        !matches!(param, Param::EqPreset | Param::EqBank) && self.is_writable(param)
    }

    fn read_allowlist(&self) -> &'static [u8] {
        &READ_ALLOWLIST
    }

    fn write_allowlist(&self) -> &'static [u8] {
        &WRITE_ALLOWLIST
    }

    fn event_only(&self) -> &'static [u8] {
        &EVENT_ONLY
    }

    fn indexed_reads(&self) -> &'static [u8] {
        &[]
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
        encode_write(id, payload).map(|b| b.to_vec())
    }

    fn parse(&self, raw: &[u8]) -> Result<Vec<ParamFrame>, ProtocolError> {
        parse(raw)
    }

    fn write_result(&self, ack: &ParamFrame) -> Option<ResultCode> {
        ack.value().map(|b| result_code(ack.param, b))
    }

    // `read_refused` keeps its default. No refused read has been observed on
    // this device, and `0xff` is a real value here: `0x13` reads it when the
    // custom EQ preset is active.
    //
    // `link_answered_without_headset` keeps its default too. With the headset
    // off, a read of `0x20` -- like every other read -- went unanswered; the
    // dongle reports the link only by the `0x20` event it pushes when the
    // headset goes away and comes back.

    fn session_open(&self) -> Option<Vec<u8>> {
        Some(session_open_report().to_vec())
    }
}

/// Synapse's `e1 01` request (family `0E`), byte for byte as captured (S26).
///
/// After the dongle is plugged in, it answers nothing but the link read until
/// a host sends this; one is enough, and the effect lasts until the dongle is
/// reset (sessions 24-26). Synapse sends it before its first read, which is
/// why everything worked while Synapse had run since the dongle was plugged
/// in. Its meaning beyond that is unknown, so it is sent only as observed.
pub fn session_open_report() -> [u8; CONTROL_REPORT_LEN] {
    const E1_REQUEST: [u8; 12] = [
        0x02, 0x80, 0x07, 0x00, 0x00, 0x50, 0x41, 0x0e, 0x00, 0x02, 0xe1, 0x01,
    ];
    let mut buf = [0u8; CONTROL_REPORT_LEN];
    buf[..E1_REQUEST.len()].copy_from_slice(&E1_REQUEST);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured input report: the bytes up to and past `LEN`, padded to 64
    /// as the device pads them. Hex as recorded in the session notes.
    fn report(hex: &str) -> [u8; CONTROL_REPORT_LEN] {
        let mut b = [0u8; CONTROL_REPORT_LEN];
        for (i, byte) in hex.split_whitespace().enumerate() {
            b[i] = u8::from_str_radix(byte, 16).unwrap();
        }
        b
    }

    fn host(hex: &str) -> Vec<u8> {
        report(hex).to_vec()
    }

    #[test]
    fn the_session_opening_request_matches_the_captured_frame() {
        // S26 15.94 s: Synapse's first e1 request on a cold dongle.
        assert_eq!(
            V2_PRO.session_open().unwrap(),
            host("02 80 07 00 00 50 41 0e 00 02 e1 01")
        );
    }

    #[test]
    fn the_answer_to_the_opening_request_carries_no_parameter() {
        // S26 15.94 s, the device's answer: a family-01 message, not decoded.
        let frames = parse(&report("02 0d 50 49 01 c0 f8 cd 01 00 03 00 0e 80 00")).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn reads_match_the_captured_frames() {
        // S08 18.15 s and S14 71.47 s.
        assert_eq!(
            encode_read(0x2C, None).unwrap().to_vec(),
            host("02 80 08 00 00 50 41 08 00 03 2c 00 00")
        );
        assert_eq!(
            encode_read(0x27, None).unwrap().to_vec(),
            host("02 80 08 00 00 50 41 08 00 03 27 00 00")
        );
    }

    #[test]
    fn writes_match_the_captured_frames() {
        // S04 28.85 s and 28.97 s: the sidetone preamble, then the level.
        assert_eq!(
            encode_write(0x18, &[0x01]).unwrap().to_vec(),
            host("02 80 09 00 00 50 41 08 00 04 98 00 01 01")
        );
        assert_eq!(
            encode_write(0x19, &[0x0D]).unwrap().to_vec(),
            host("02 80 09 00 00 50 41 08 00 04 99 00 01 0d")
        );
        // S08 18.28 s: power saving on at 15 minutes.
        assert_eq!(
            encode_write(0x2C, &[0x0F]).unwrap().to_vec(),
            host("02 80 09 00 00 50 41 08 00 04 ac 00 01 0f")
        );
    }

    #[test]
    fn the_ten_byte_curve_matches_the_captured_frame() {
        // S12 17.14 s: band 1 at -5, the rest 0.
        let mut bands = [0u8; 10];
        bands[0] = 0xFB;
        assert_eq!(
            encode_write(EQ_BANDS, &bands).unwrap().to_vec(),
            host("02 80 12 00 00 50 41 08 00 0d 95 00 0a fb 00 00 00 00 00 00 00 00 00")
        );
    }

    #[test]
    fn a_read_reply_is_decoded_and_the_stale_tail_ignored() {
        // S08: 0x2c read back as 0x0f. Everything after byte 15 is left over
        // from an earlier report.
        let r = report(
            "02 0e 50 49 08 f9 2a a7 05 00 04 00 2c 01 01 0f 49 01 c0 80 0f 35 03 03 00 11 ae 13",
        );
        assert_eq!(
            parse(&r).unwrap(),
            vec![ParamFrame {
                param: 0x2C,
                is_write: false,
                role: Role::Response,
                payload: vec![0x0F],
            }]
        );
    }

    #[test]
    fn a_write_ack_carries_the_write_bit() {
        // S14 71.60 s: the do-not-disturb write acknowledged with 00.
        let r = report("02 0e 50 49 08 c2 7d e0 0a 00 04 00 a7 01 01 00 49 01 c0 22");
        let f = &parse(&r).unwrap()[0];
        assert_eq!(f.param, 0x27);
        assert!(f.is_write);
        assert_eq!(V2_PRO.write_result(f), Some(ResultCode::Ok));
    }

    #[test]
    fn events_are_decoded_with_their_role() {
        for (hex, param, value) in [
            // S14: do not disturb switched on.
            (
                "02 0e 50 49 08 c3 7d e0 0a 00 04 00 27 02 01 01",
                0x27,
                0x01,
            ),
            // S04: battery pushed at 53%.
            (
                "02 0e 50 49 08 d5 f5 0b 01 00 04 00 21 02 01 35",
                0x21,
                0x35,
            ),
            // S19: hardware mute.
            (
                "02 0e 50 49 08 ef cf 56 0f 00 04 00 55 02 01 01",
                0x55,
                0x01,
            ),
            // S19: the headset cycled its EQ to game.
            (
                "02 0e 50 49 08 ed 4e 98 0f 00 04 00 13 02 01 07",
                0x13,
                0x07,
            ),
        ] {
            let f = &parse(&report(hex)).unwrap()[0];
            assert_eq!(
                (f.param, f.role, f.value()),
                (param, Role::Event, Some(value))
            );
        }
    }

    #[test]
    fn concatenated_messages_are_each_decoded() {
        // S03 11.84 s: the link-down event, then a family-11 message that is
        // not decoded. Exactly one parameter frame comes out.
        let r = report(
            "02 1c 50 49 08 dd f6 7c 2f 03 04 00 20 02 01 00 \
             50 49 11 de f7 7c 2f 03 04 00 04 00 00 32 00 02 06 0b",
        );
        let frames = parse(&r).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].param, 0x20);
        assert_eq!(frames[0].role, Role::Event);
        assert_eq!(frames[0].value(), Some(0x00));
    }

    #[test]
    fn a_report_of_another_family_alone_yields_nothing() {
        // The once-a-second unsolicited report, family 01.
        let r = report("02 0d 50 49 01 c0 3a 58 2f 03 03 00 11 f0 13");
        assert!(parse(&r).unwrap().is_empty());
    }

    #[test]
    fn lengths_that_do_not_add_up_are_refused() {
        // Declared total longer than the report.
        assert!(matches!(
            parse(&report("02 3f 50 49 08")),
            Err(ProtocolError::ImplausibleDataSize { .. })
        ));
        // Body runs past the declared total.
        assert!(matches!(
            parse(&report("02 0e 50 49 08 c3 7d e0 0a 00 09 00 27 02 01 01")),
            Err(ProtocolError::MalformedMessage { .. })
        ));
        // Payload length disagrees with the body length.
        assert!(matches!(
            parse(&report("02 0e 50 49 08 c3 7d e0 0a 00 04 00 27 02 02 01")),
            Err(ProtocolError::LengthDisagreement { .. })
        ));
        // Wrong marker.
        assert!(matches!(
            parse(&report("02 0e 50 41 08 c3 7d e0 0a 00 04 00 27 02 01 01")),
            Err(ProtocolError::MalformedMessage { .. })
        ));
        // A role never observed from the device.
        assert!(matches!(
            parse(&report("02 0e 50 49 08 c3 7d e0 0a 00 04 00 27 00 01 01")),
            Err(ProtocolError::UnknownRole { role: 0 })
        ));
    }

    #[test]
    fn unobserved_identifiers_cannot_be_encoded() {
        assert!(matches!(
            encode_read(0x33, None),
            Err(ProtocolError::NotAllowlisted { write: false, .. })
        ));
        assert!(matches!(
            encode_write(0x21, &[1]),
            Err(ProtocolError::NotAllowlisted { write: true, .. })
        ));
        // Mic monitoring level was written, never read.
        assert!(encode_read(0x19, None).is_err());
    }

    #[test]
    fn the_serial_cannot_be_read() {
        // Observed, and excluded on purpose: it returns the device serial.
        assert!(matches!(
            encode_read(0x00, None),
            Err(ProtocolError::NotAllowlisted { param: 0x00, .. })
        ));
    }

    #[test]
    fn payload_lengths_are_enforced() {
        assert!(matches!(
            encode_write(EQ_BANDS, &[0]),
            Err(ProtocolError::WrongWritePayloadLen { expected: 10, .. })
        ));
        assert!(matches!(
            encode_write(0x19, &[1, 2]),
            Err(ProtocolError::WrongWritePayloadLen { expected: 1, .. })
        ));
        assert!(matches!(
            encode_read(0x21, Some(1)),
            Err(ProtocolError::UnexpectedIndex { .. })
        ));
    }

    #[test]
    fn every_write_round_trips_through_a_device_shaped_ack() {
        // The device echoes the command byte, write bit included.
        for id in WRITE_ALLOWLIST {
            let mut r = [0u8; CONTROL_REPORT_LEN];
            r[..16].copy_from_slice(&[
                0x02,
                0x0E,
                0x50,
                0x49,
                0x08,
                0xC0,
                0,
                0,
                0,
                0,
                0x04,
                0x00,
                id | 0x80,
                0x01,
                0x01,
                0x00,
            ]);
            let f = &parse(&r).unwrap()[0];
            assert_eq!((f.param, f.is_write), (id, true));
        }
    }

    #[test]
    fn status_01_counts_as_success_only_where_it_was_observed_with_success() {
        assert_eq!(result_code(0x16, 0x01), ResultCode::Ok);
        assert_eq!(result_code(0x13, 0x01), ResultCode::Ok);
        assert_eq!(result_code(0x19, 0x01), ResultCode::Unknown(0x01));
        assert_eq!(result_code(0x19, 0xFF), ResultCode::Unknown(0xFF));
        assert_eq!(result_code(0x19, 0x00), ResultCode::Ok);
    }

    #[test]
    fn the_named_parameters_map_as_the_research_record_says() {
        let mapped: Vec<(&str, u8)> = Param::ALL
            .into_iter()
            .filter_map(|p| param_id(p).map(|id| (p.name(), id)))
            .collect();
        assert_eq!(
            mapped,
            vec![
                ("link", 0x20),
                ("battery", 0x21),
                ("sidetone", 0x19),
                ("mic-mute", 0x55),
                ("power-saving", 0x2C),
                ("do-not-disturb", 0x27),
                ("eq-preset", 0x13),
                ("eq-bank", 0x1D),
                ("mic-eq", 0x16),
            ]
        );
    }

    #[test]
    fn readability_follows_what_was_observed() {
        let readable: Vec<&str> = Param::ALL
            .into_iter()
            .filter(|p| V2_PRO.is_readable(*p))
            .map(|p| p.name())
            .collect();
        // Sidetone and mic EQ were only written; mic mute only pushed.
        assert_eq!(
            readable,
            vec![
                "link",
                "battery",
                "power-saving",
                "do-not-disturb",
                "eq-preset",
                "eq-bank"
            ]
        );
    }

    #[test]
    fn values_are_limited_to_the_observed_ones() {
        assert!(V2_PRO.accepts(Param::PowerSavingTimeout, 0));
        assert!(V2_PRO.accepts(Param::PowerSavingTimeout, 15));
        assert!(V2_PRO.accepts(Param::PowerSavingTimeout, 60));
        assert!(!V2_PRO.accepts(Param::PowerSavingTimeout, 14));
        assert!(!V2_PRO.accepts(Param::PowerSavingTimeout, 61));
        assert!(V2_PRO.accepts(Param::Sidetone, 15));
        assert!(!V2_PRO.accepts(Param::Sidetone, 16));
        assert!(V2_PRO.accepts(Param::MicEqMode, 0xFF));
        assert!(!V2_PRO.accepts(Param::MicEqMode, 0x04));
        assert!(!V2_PRO.accepts(Param::DoNotDisturb, 2));
    }

    #[test]
    fn eq_preset_and_bank_cannot_be_written_on_their_own() {
        assert!(V2_PRO.is_writable(Param::EqPreset));
        assert!(!V2_PRO.standalone_write(Param::EqPreset));
        assert!(!V2_PRO.standalone_write(Param::EqBank));
        assert!(V2_PRO.standalone_write(Param::Sidetone));
        assert!(!V2_PRO.standalone_write(Param::Battery));
    }

    #[test]
    fn the_dongle_does_not_answer_for_an_absent_headset() {
        assert!(!V2_PRO.link_answered_without_headset());
        assert!(crate::v3pro::V3_PRO.link_answered_without_headset());
    }

    #[test]
    fn ff_is_a_value_here_not_a_refusal() {
        let reply = ParamFrame {
            param: 0x13,
            is_write: false,
            role: Role::Response,
            payload: vec![0xFF],
        };
        assert!(!V2_PRO.read_refused(Param::EqPreset, &reply));
    }

    #[test]
    fn sidetone_carries_the_observed_preamble() {
        assert_eq!(V2_PRO.write_preamble(Param::Sidetone), Some((0x18, 1)));
        assert_eq!(V2_PRO.write_preamble(Param::PowerSavingTimeout), None);
    }
}
