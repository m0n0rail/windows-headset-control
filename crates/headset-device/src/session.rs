//! Device resolution and request/response correlation for the control channel.
//!
//! Two problems live here that no caller should have to solve twice.
//!
//! **Resolution.** A candidate index and a device path are both machine-local:
//! the control collection sits at a different absolute index on a different PC,
//! and the path embeds an instance id that changes when the dongle is replugged.
//! Nothing here persists either. Every session re-resolves by identity.
//!
//! **Correlation.** The device pushes unsolicited events — battery, mute, wheel
//! movement — on the same collection that carries responses. A naive
//! read-one-report would let a battery event satisfy a sidetone read. The
//! session matches on the command bytes and the response role, and routes
//! everything else to an event sink.

use std::time::{Duration, Instant};

use headset_protocol::{Codec, ParamFrame, ProtocolError, Role};

use crate::backend::{HidBackend, HidTransport};
use crate::error::DeviceError;
use crate::model::{CollectionInfo, OpenMode};
use crate::profile::{profile_for, ControlShape, DeviceProfile};

/// Minimum spacing between requests. Reserved in Phase 1; enforced here.
pub const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(250);

/// How long a single exchange may take, including any events that arrive while
/// waiting. Applies to the exchange as a whole, so a talkative device cannot
/// extend it indefinitely one event at a time.
pub const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Per-read timeout inside an exchange. Shorter than the exchange budget so a
/// silent device is noticed without waiting out the whole deadline.
const READ_SLICE: Duration = Duration::from_millis(250);

/// A control collection found by identity, with the profile that identified it.
#[derive(Clone, Debug)]
pub struct ResolvedControl {
    pub info: CollectionInfo,
    pub profile: &'static DeviceProfile,
}

/// Finds the control collection by identity, never by index or stored path.
///
/// A collection qualifies when its product has a profile and its usage page is
/// the one that profile's protocol was measured on. Its report lengths are then
/// checked against the same profile, so no device is written to with framing
/// derived from another.
pub fn resolve_control(backend: &dyn HidBackend) -> Result<ResolvedControl, DeviceError> {
    let all = backend.enumerate()?;
    let matches: Vec<(&CollectionInfo, &'static DeviceProfile)> = all
        .iter()
        .filter_map(|c| profile_for(c).map(|p| (c, p)))
        .filter(|(c, p)| c.usage_page == p.control.usage_page)
        .collect();

    match matches.as_slice() {
        [] => Err(DeviceError::DongleNotFound),
        [(c, p)] => {
            check_shape(c, &p.control)?;
            Ok(ResolvedControl {
                info: (*c).clone(),
                profile: p,
            })
        }
        // Two dongles is a real configuration, not a bug. Refuse rather than
        // pick one, because picking silently would write to whichever happened
        // to enumerate first.
        many => Err(DeviceError::AmbiguousDevice(many.len())),
    }
}

/// [`resolve_control`] for callers that need only the collection.
pub fn resolve_control_device(backend: &dyn HidBackend) -> Result<CollectionInfo, DeviceError> {
    resolve_control(backend).map(|r| r.info)
}

fn check_shape(c: &CollectionInfo, want: &ControlShape) -> Result<(), DeviceError> {
    let mut wrong = Vec::new();
    if c.input_report_len != want.input_report_len {
        wrong.push(format!(
            "input report {} bytes, expected {}",
            c.input_report_len, want.input_report_len
        ));
    }
    if c.output_report_len != want.output_report_len {
        wrong.push(format!(
            "output report {} bytes, expected {}",
            c.output_report_len, want.output_report_len
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(DeviceError::UnexpectedControlShape(wrong.join("; ")))
    }
}

/// An open control channel that can exchange parameter requests.
pub struct ControlSession {
    transport: Box<dyn HidTransport>,
    info: CollectionInfo,
    profile: &'static DeviceProfile,
    last_request: Option<Instant>,
    exchange_timeout: Duration,
    /// Events observed while waiting for responses, oldest first.
    pending_events: Vec<ParamFrame>,
}

impl ControlSession {
    /// Resolves the device and opens it for read and write.
    pub fn open(backend: &dyn HidBackend) -> Result<Self, DeviceError> {
        let ResolvedControl { info, profile } = resolve_control(backend)?;
        let transport = backend.open(&info.id, OpenMode::ReadWrite)?;
        Ok(Self {
            transport,
            info,
            profile,
            last_request: None,
            exchange_timeout: DEFAULT_EXCHANGE_TIMEOUT,
            pending_events: Vec::new(),
        })
    }

    /// Opens read-only, for listening without the ability to write.
    pub fn open_read_only(backend: &dyn HidBackend) -> Result<Self, DeviceError> {
        let ResolvedControl { info, profile } = resolve_control(backend)?;
        let transport = backend.open(&info.id, OpenMode::Read)?;
        Ok(Self {
            transport,
            info,
            profile,
            last_request: None,
            exchange_timeout: DEFAULT_EXCHANGE_TIMEOUT,
            pending_events: Vec::new(),
        })
    }

    pub fn with_exchange_timeout(mut self, t: Duration) -> Self {
        self.exchange_timeout = t;
        self
    }

    pub fn info(&self) -> &CollectionInfo {
        &self.info
    }

    /// The profile the device was identified by.
    pub fn profile(&self) -> &'static DeviceProfile {
        self.profile
    }

    /// The protocol of the device this session is open on. Every request sent
    /// through [`ControlSession::exchange`] should be encoded with it.
    pub fn codec(&self) -> &'static dyn Codec {
        self.profile.codec
    }

    /// Events collected while waiting for responses. Draining hands ownership to
    /// the caller so a long-lived session does not accumulate them forever.
    pub fn drain_events(&mut self) -> Vec<ParamFrame> {
        std::mem::take(&mut self.pending_events)
    }

    /// Sends an encoded request and waits for the response that answers it.
    ///
    /// `param` and `is_write` describe the request, and only a frame matching
    /// both — with role `Response` — resolves the wait. Events are collected and
    /// do not satisfy it.
    pub fn exchange(
        &mut self,
        request: &[u8],
        param: u8,
        is_write: bool,
    ) -> Result<ParamFrame, DeviceError> {
        self.pace();
        self.transport.write_report(request)?;
        self.last_request = Some(Instant::now());

        let deadline = Instant::now() + self.exchange_timeout;
        let mut events_seen = 0usize;
        let mut buf = vec![0u8; self.transport.input_report_len() as usize];

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .transport
                .read_report(&mut buf, READ_SLICE.min(remaining))
            {
                Ok(n) => match self.profile.codec.parse(&buf[..n]) {
                    Ok(frames) => {
                        // Every frame in the report is looked at before
                        // returning, so an event that shares a report with our
                        // response is still delivered.
                        let mut answer = None;
                        for frame in frames {
                            if answer.is_none()
                                && frame.role == Role::Response
                                && frame.param == param
                                // A write and a read of the same parameter are
                                // distinct commands on the wire, so matching the
                                // parameter alone would let a read response
                                // satisfy a write that was issued alongside it.
                                && frame.is_write == is_write
                            {
                                answer = Some(frame);
                            } else if frame.role == Role::Event {
                                events_seen += 1;
                                self.pending_events.push(frame);
                            }
                            // A response for something else is dropped: it
                            // belongs to another writer on this collection.
                        }
                        // A report holding nothing of ours -- including one
                        // outside the parameter family -- is not fatal: another
                        // process shares this collection. Keep waiting.
                        if let Some(frame) = answer {
                            return Ok(frame);
                        }
                    }
                    // Nor is a frame that fails validation, for the same reason.
                    Err(ProtocolError::ChecksumMismatch { .. }) => {
                        tracing::debug!("discarding a frame with a bad checksum");
                    }
                    Err(e) => {
                        tracing::debug!("discarding an undecodable frame: {e}");
                    }
                },
                Err(DeviceError::Timeout(_)) => continue,
                Err(e) => return Err(e),
            }
        }

        Err(DeviceError::NoResponse {
            param,
            waited: self.exchange_timeout,
            events_seen,
        })
    }

    /// Reads input reports until the deadline, collecting decoded events.
    ///
    /// Sends nothing. This is the listen path the tray's reader thread and
    /// `headsetctl watch` share.
    pub fn listen(&mut self, duration: Duration) -> Result<Vec<ParamFrame>, DeviceError> {
        let deadline = Instant::now() + duration;
        let mut out = std::mem::take(&mut self.pending_events);
        let mut buf = vec![0u8; self.transport.input_report_len() as usize];

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .transport
                .read_report(&mut buf, READ_SLICE.min(remaining))
            {
                Ok(n) => {
                    if let Ok(frames) = self.profile.codec.parse(&buf[..n]) {
                        out.extend(frames.into_iter().filter(|f| f.role == Role::Event));
                    }
                }
                Err(DeviceError::Timeout(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Honours `MIN_REQUEST_INTERVAL` between consecutive requests.
    fn pace(&self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                std::thread::sleep(MIN_REQUEST_INTERVAL - elapsed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeHidBackend;
    use headset_protocol::v3pro::{checksum, encode_read, param_id};
    use headset_protocol::{Param, CONTROL_REPORT_LEN};

    const FIXTURE: &str = include_str!("../tests/fixtures/blackshark-v3-pro-ps.json");
    const INTERLOPER: &str = include_str!("../tests/fixtures/blackshark-plus-interloper.json");

    fn response(param: u8, is_write: bool, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; CONTROL_REPORT_LEN];
        b[0] = 0x02;
        b[1] = 0x02;
        b[2] = 0x60;
        b[6] = 4 + payload.len() as u8;
        b[9] = 0x80;
        b[10] = if is_write { param | 0x80 } else { param };
        b[11] = 0x01;
        b[12] = payload.len() as u8;
        b[13..13 + payload.len()].copy_from_slice(payload);
        b[62] = checksum(&b);
        b
    }

    fn event(param: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = response(param, false, payload);
        b[9] = 0x00;
        b[11] = 0x02;
        b[62] = 0;
        b[62] = checksum(&b);
        b
    }

    #[test]
    fn resolves_the_control_collection_by_identity() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        assert_eq!(info.usage_page, 0xFF14);
        assert_eq!(info.input_report_len, 64);
    }

    #[test]
    fn resolution_names_the_profile_that_identified_the_device() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let r = resolve_control(&b).unwrap();
        assert_eq!(r.profile.product_id, 0x101B);
        assert_eq!(r.profile.codec.name(), "blackshark-v3-pro");
    }

    #[test]
    fn a_control_collection_of_the_wrong_shape_is_refused_not_written_to() {
        // The profile's usage page matches but the report width does not: the
        // framing was measured at 64 bytes, so nothing is sent at 60.
        let narrowed = FIXTURE.replace(
            r#""input_report_len": 64, "output_report_len": 64"#,
            r#""input_report_len": 60, "output_report_len": 64"#,
        );
        assert_ne!(narrowed, FIXTURE, "the fixture edit must have applied");
        let b = FakeHidBackend::from_fixture_str(&narrowed).unwrap();
        match resolve_control(&b) {
            Err(DeviceError::UnexpectedControlShape(why)) => {
                assert!(why.contains("input report 60 bytes, expected 64"), "{why}");
            }
            other => panic!("expected a shape refusal, got {other:?}"),
        }
        assert!(ControlSession::open(&b).is_err());
    }

    #[test]
    fn resolution_ignores_an_unrelated_vendors_wider_collection() {
        // The interloper fixture carries a non-Razer collection that outscores
        // the headset on shape alone. Identity-based resolution must not pick
        // it, which is the failure mode `list` and `probe` both had.
        let b = FakeHidBackend::from_fixture_str(INTERLOPER).unwrap();
        let info = resolve_control_device(&b).unwrap();
        assert_eq!(info.vendor_id, 0x1532);
    }

    #[test]
    fn a_read_exchange_returns_the_matching_response() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        b.push_read(&info.id, response(param_id(Param::Battery), false, &[52]));

        let mut s = ControlSession::open(&b).unwrap();
        let req = encode_read(param_id(Param::Battery), None).unwrap();
        let f = s.exchange(&req, param_id(Param::Battery), false).unwrap();
        assert_eq!(f.value(), Some(52));
    }

    #[test]
    fn an_event_does_not_satisfy_a_pending_read() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        // A battery event arrives first; the sidetone response follows.
        b.push_read(&info.id, event(param_id(Param::Battery), &[49]));
        b.push_read(&info.id, response(param_id(Param::Sidetone), false, &[7]));

        let mut s = ControlSession::open(&b).unwrap();
        let req = encode_read(param_id(Param::Sidetone), None).unwrap();
        let f = s.exchange(&req, param_id(Param::Sidetone), false).unwrap();
        assert_eq!(f.value(), Some(7), "the event must not have answered this");

        let events = s.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].param, param_id(Param::Battery));
        assert_eq!(events[0].value(), Some(49));
    }

    #[test]
    fn a_read_response_does_not_satisfy_a_write() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        // Same parameter, but the read form. It must not resolve a write.
        b.push_read(&info.id, response(param_id(Param::Sidetone), false, &[7]));

        let mut s = ControlSession::open(&b)
            .unwrap()
            .with_exchange_timeout(Duration::from_millis(300));
        let req = encode_read(param_id(Param::Sidetone), None).unwrap();
        assert!(s.exchange(&req, param_id(Param::Sidetone), true).is_err());
    }

    #[test]
    fn a_silent_device_times_out_with_a_useful_message() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let mut s = ControlSession::open(&b)
            .unwrap()
            .with_exchange_timeout(Duration::from_millis(300));
        let req = encode_read(param_id(Param::Battery), None).unwrap();
        let err = s
            .exchange(&req, param_id(Param::Battery), false)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no response"), "{msg}");
        assert!(msg.contains("0 unrelated event"), "{msg}");

        // The variant matters as much as the wording. Silence used to arrive as
        // `ProtocolMismatch`, which says the device answered wrongly, and a
        // caller that wanted to act on "answered nothing" had no choice but to
        // match on this string.
        match err {
            DeviceError::NoResponse {
                param,
                waited,
                events_seen,
            } => {
                assert_eq!(param, param_id(Param::Battery));
                assert_eq!(waited, Duration::from_millis(300));
                assert_eq!(events_seen, 0);
            }
            other => panic!("silence should be NoResponse, got {other:?}"),
        }
    }

    #[test]
    fn the_request_reaches_the_wire_verbatim() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        b.push_read(&info.id, response(param_id(Param::Battery), false, &[52]));

        let mut s = ControlSession::open(&b).unwrap();
        let req = encode_read(param_id(Param::Battery), None).unwrap();
        s.exchange(&req, param_id(Param::Battery), false).unwrap();

        let writes = b.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0], req.to_vec());
    }

    #[test]
    fn a_read_only_session_cannot_write() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let mut s = ControlSession::open_read_only(&b).unwrap();
        let req = encode_read(param_id(Param::Battery), None).unwrap();
        assert!(matches!(
            s.exchange(&req, param_id(Param::Battery), false),
            Err(DeviceError::WriteNotSupported)
        ));
    }

    #[test]
    fn listen_collects_only_events() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        b.push_read(&info.id, event(param_id(Param::MicMute), &[1]));
        b.push_read(&info.id, response(param_id(Param::Battery), false, &[52]));
        b.push_read(&info.id, event(param_id(Param::Sidetone), &[3]));

        let mut s = ControlSession::open_read_only(&b).unwrap();
        let events = s.listen(Duration::from_millis(400)).unwrap();
        assert_eq!(
            events.len(),
            2,
            "the response must not be reported as an event"
        );
        assert_eq!(events[0].param, param_id(Param::MicMute));
        assert_eq!(events[1].param, param_id(Param::Sidetone));
    }
}
