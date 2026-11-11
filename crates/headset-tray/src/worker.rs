//! The device thread.
//!
//! Owns the one `ControlSession` and is the only thing that talks to the
//! headset. The UI thread sends `Command`s and receives `HeadsetState`
//! snapshots; it never blocks on HID I/O, which is what keeps the tray menu
//! responsive while an exchange is in flight.
//!
//! Reconnect is handled here too: if the session dies (the dongle re-enumerates
//! on replug, and its path changes when it does) the thread re-resolves rather
//! than exiting.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use headset_device::{ControlSession, DeviceError, HidBackend};
use headset_protocol::v3pro::noise::{noise_write_payload, NoiseControl};
use headset_protocol::{Codec, Param, ParamFrame, ResultCode};

use crate::state::{Features, HeadsetState};

/// How long to sit in `listen` between polls. Events arrive on their own, so
/// this is not a polling interval for values — it is how often the thread wakes
/// to notice commands and connection changes.
const LISTEN_SLICE: Duration = Duration::from_millis(500);

/// How often to re-read everything even without an event, as a backstop against
/// a missed push. Deliberately slow: the device pushes changes, so this exists
/// only to heal a missed one, not to drive the UI.
const FULL_REFRESH: Duration = Duration::from_secs(60);

/// Consecutive silent refreshes before the tray calls the dongle unresponsive.
/// One missed exchange is ordinary. Three in a row on `0x20`, which the dongle
/// answers out of its own state rather than proxying to the headset, is not.
const SILENCE_THRESHOLD: u32 = 3;

/// How soon to retry a refresh that failed without killing the session.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(2);

/// Retry interval once the dongle has been declared unresponsive. A wedged
/// dongle stays wedged until it is replugged and every attempt costs a full
/// exchange timeout, so asking constantly buys nothing and keeps a thread busy.
const SILENT_RETRY: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SetSidetone(u8),
    SetGameChat(u8),
    /// Both bytes of parameter `0x12`. Never one: the device holds mode and
    /// level together, so the caller composes the whole state.
    SetNoise(NoiseControl),
    Refresh,
    Shutdown,
}

/// Runs until a `Shutdown` command arrives or the command channel closes.
///
/// `notify` is called after every state change so the UI can repaint. It is
/// invoked from this thread, so implementations must be cheap and thread-safe —
/// posting a window message is the intended use.
pub fn run<B: HidBackend, F: Fn(HeadsetState)>(
    backend: &B,
    commands: Receiver<Command>,
    notify: F,
) {
    // Device-owned state only. `warn_vendor_software` and `mic_mute_os` belong
    // to the UI thread; setting them here would be a value this thread has no
    // business having an opinion about, and it used to be computed once at
    // startup and then quietly stamped over the user's choice.
    let mut state = HeadsetState::default();
    let mut session: Option<ControlSession> = None;
    let mut since_refresh = Duration::ZERO;
    // How long to wait before the next `refresh_all`, which varies with the
    // last outcome rather than being a single constant: a healthy device needs
    // only the slow backstop, a transiently failing one should be retried
    // sooner, and a silent one should be left mostly alone.
    let mut refresh_due = Duration::ZERO;
    let mut silent_reads: u32 = 0;

    loop {
        // (Re)establish the session. A failure here is normal, not fatal: the
        // dongle may be unplugged, so back off and try again rather than exit.
        if session.is_none() {
            match ControlSession::open(backend) {
                Ok(s) => {
                    // Take the name from the descriptor the moment the device
                    // resolves, so the header is right before any read lands.
                    state.device_name = s.info().product.clone();
                    state.model = Some(s.profile().name);
                    state.features = Features::of(s.codec());
                    session = Some(s);
                    since_refresh = Duration::ZERO;
                    refresh_due = Duration::ZERO; // read immediately
                }
                Err(e) => {
                    tracing::debug!("control session unavailable: {e}");
                    state.connected = None;
                    // Absent is not silent. There is no dongle to advise
                    // replugging, and leaving the flag set would keep that
                    // advice on screen after the dongle was pulled.
                    state.dongle_silent = false;
                    silent_reads = 0;
                    notify(state.clone());
                    match commands.recv_timeout(Duration::from_secs(3)) {
                        Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                        _ => continue,
                    }
                }
            }
        }
        let Some(s) = session.as_mut() else { continue };

        if since_refresh >= refresh_due {
            since_refresh = Duration::ZERO;
            match refresh_all(s, &mut state) {
                Ok(()) => {
                    silent_reads = 0;
                    state.dongle_silent = false;
                    refresh_due = FULL_REFRESH;
                    notify(state.clone());
                }
                Err(e) if is_fatal(&e) => {
                    tracing::warn!("session lost, will re-resolve: {e}");
                    session = None;
                    continue;
                }
                Err(e) => {
                    // Silence is the wedged-dongle signature and is reported.
                    // Any other non-fatal error is still just logged: it means
                    // the device answered, so it is talking.
                    if matches!(e, DeviceError::NoResponse { .. })
                        && !s.codec().link_answered_without_headset()
                    {
                        // On this device the dongle does not answer for itself,
                        // so silence is what a headset that is off or out of
                        // range looks like. Advising a replug would be wrong
                        // every time the headset is simply switched off.
                        if state.connected != Some(false) {
                            state.mark_disconnected();
                            notify(state.clone());
                        }
                    } else if matches!(e, DeviceError::NoResponse { .. }) {
                        silent_reads = silent_reads.saturating_add(1);
                        if silent_reads >= SILENCE_THRESHOLD && !state.dongle_silent {
                            tracing::warn!(
                                "dongle answered none of the last {silent_reads} reads; \
                                 reporting it as unresponsive"
                            );
                            state.dongle_silent = true;
                            notify(state.clone());
                        }
                    }
                    refresh_due = if state.dongle_silent {
                        SILENT_RETRY
                    } else {
                        RETRY_AFTER_FAILURE
                    };
                    tracing::debug!("refresh failed: {e}");
                }
            }
        }

        match commands.recv_timeout(LISTEN_SLICE) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(cmd) => {
                if let Err(e) = apply(s, cmd, &mut state) {
                    if is_fatal(&e) {
                        session = None;
                        continue;
                    }
                    if matches!(e, DeviceError::NoResponse { .. })
                        && !s.codec().link_answered_without_headset()
                    {
                        // Same reading as the timed refresh: on this device,
                        // silence is a headset that is off. Expected, so not a
                        // warning, and the panel says so.
                        state.mark_disconnected();
                        tracing::debug!("command unanswered; headset unreachable: {e}");
                    } else {
                        tracing::warn!("command failed: {e}");
                    }
                }
                notify(state.clone());
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // No command pending: spend the slice listening for pushes.
        match s.listen(LISTEN_SLICE) {
            Ok(events) => {
                if !events.is_empty() {
                    let codec = s.codec();
                    for e in &events {
                        state.apply(codec, e);
                    }
                    // A link transition invalidates everything; re-read rather
                    // than wait for individual pushes that may not come.
                    if events
                        .iter()
                        .any(|e| codec.param_for(e.param) == Some(Param::LinkState))
                    {
                        since_refresh = FULL_REFRESH;
                    }
                    notify(state.clone());
                }
            }
            Err(e) if is_fatal(&e) => {
                session = None;
                continue;
            }
            Err(e) => tracing::debug!("listen failed: {e}"),
        }
        since_refresh += LISTEN_SLICE;
    }
}

/// Errors that mean the handle is gone rather than the device being slow.
fn is_fatal(e: &DeviceError) -> bool {
    matches!(
        e,
        DeviceError::DisconnectedDuringOp
            | DeviceError::DongleNotFound
            | DeviceError::AccessDenied
            | DeviceError::Os(_)
    )
}

/// Reads one named parameter into `state`. A parameter the device has no
/// identifier for, or was never observed answering a read of, is left as it is
/// rather than read under another's id or with an invented request.
fn read_into(
    s: &mut ControlSession,
    param: Param,
    state: &mut HeadsetState,
) -> Result<(), DeviceError> {
    let codec = s.codec();
    let Some(id) = codec.id(param).filter(|_| codec.is_readable(param)) else {
        return Ok(());
    };
    let req = codec
        .encode_read(id, None)
        .expect("a codec's named parameters are on its read allowlist");
    let frame = s.exchange(&req, id, false)?;
    state.apply(codec, &frame);
    Ok(())
}

/// Writes one named parameter, preceded by whatever the device was observed
/// receiving before it, and rejects anything but an acknowledged write.
fn write_param(s: &mut ControlSession, param: Param, payload: &[u8]) -> Result<(), DeviceError> {
    let codec = s.codec();
    let id = codec.id(param).ok_or_else(|| {
        DeviceError::ProtocolMismatch(format!(
            "{} has no identifier on the {}",
            param.name(),
            s.profile().name
        ))
    })?;
    if let Some((pre_id, pre_value)) = codec.write_preamble(param) {
        let pre = codec
            .encode_write(pre_id, &[pre_value])
            .expect("allowlisted");
        let ack = s.exchange(&pre, pre_id, true)?;
        reject_unless_ok(codec, &ack)?;
    }
    let req = codec
        .encode_write(id, payload)
        .map_err(|e| DeviceError::ProtocolMismatch(e.to_string()))?;
    let ack = s.exchange(&req, id, true)?;
    reject_unless_ok(codec, &ack)
}

fn refresh_all(s: &mut ControlSession, state: &mut HeadsetState) -> Result<(), DeviceError> {
    // Link first: if the headset is unreachable every other read returns the
    // refusal byte, and `apply` would have to discard them all anyway.
    read_into(s, Param::LinkState, state)?;
    if state.connected == Some(false) {
        return Ok(());
    }
    for p in [
        Param::Battery,
        Param::Sidetone,
        Param::GameChatBalance,
        Param::MicMute,
        Param::NoiseCancellation,
    ] {
        read_into(s, p, state)?;
    }
    Ok(())
}

fn apply(
    s: &mut ControlSession,
    cmd: Command,
    state: &mut HeadsetState,
) -> Result<(), DeviceError> {
    match cmd {
        Command::Shutdown => Ok(()),
        Command::Refresh => refresh_all(s, state),
        Command::SetSidetone(v) => {
            write_param(s, Param::Sidetone, &[v])?;
            // Read back rather than assuming: the device is the source of truth.
            if s.codec().is_readable(Param::Sidetone) {
                return read_into(s, Param::Sidetone, state);
            }
            // No read of it was ever observed on this device. Show what was
            // sent and acknowledged, marked as such, rather than either
            // inventing a read or leaving the slider blank after a change.
            state.sidetone = Some(v);
            state.sidetone_unconfirmed = true;
            Ok(())
        }
        Command::SetGameChat(v) => {
            write_param(s, Param::GameChatBalance, &[v])?;
            read_into(s, Param::GameChatBalance, state)
        }
        Command::SetNoise(want) => {
            // No preamble was ever observed for this parameter, unlike
            // sidetone's. The vendor software's read before the write is the
            // UI's job here: it composed `want` from the state it holds.
            let payload = noise_write_payload(want)
                .map_err(|e| DeviceError::ProtocolMismatch(e.to_string()))?;
            write_param(s, Param::NoiseCancellation, &payload)?;
            read_into(s, Param::NoiseCancellation, state)
        }
    }
}

fn reject_unless_ok(codec: &dyn Codec, ack: &ParamFrame) -> Result<(), DeviceError> {
    match codec.write_result(ack) {
        Some(ResultCode::Ok) | None => Ok(()),
        Some(ResultCode::Refused) => Err(DeviceError::ProtocolMismatch(
            "device refused the write (0xff); the mic mute switch or an unreachable \
             headset are the observed causes"
                .into(),
        )),
        Some(ResultCode::Unknown(b)) => Err(DeviceError::ProtocolMismatch(format!(
            "unrecognised result {b:#04x}; not treating it as success"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headset_device::{resolve_control_device, FakeHidBackend};
    use headset_protocol::v3pro::noise::NoiseMode;
    use headset_protocol::v3pro::{checksum, param_id};
    use headset_protocol::CONTROL_REPORT_LEN;
    use std::sync::mpsc;

    const FIXTURE: &str =
        include_str!("../../headset-device/tests/fixtures/blackshark-v3-pro-ps.json");

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

    /// Runs the worker to completion with `Shutdown` already queued, returning
    /// every state it published.
    ///
    /// Collects into a shared `Vec` rather than a channel on purpose: a channel
    /// receiver blocks until every sender is dropped, and a notify closure that
    /// captures the sender by reference keeps one alive for the whole test. That
    /// deadlocks the test, not the code under test.
    fn run_to_completion(b: &FakeHidBackend) -> Vec<HeadsetState> {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Shutdown).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        run(b, rx, move |s| sink.lock().unwrap().push(s));
        let out = seen.lock().unwrap().clone();
        out
    }

    #[test]
    fn a_disconnected_headset_short_circuits_the_refresh() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        b.push_read(
            &info.id,
            response(param_id(Param::LinkState), false, &[0x00, 0x00]),
        );

        let states = run_to_completion(&b);
        let last = states.last().expect("at least one update");
        assert_eq!(last.connected, Some(false));
        assert_eq!(last.battery, None);
        // Link read only: the four proxied reads must be skipped.
        assert_eq!(b.writes().len(), 1);
    }

    #[test]
    fn a_full_refresh_reads_every_displayed_parameter() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        for r in [
            response(param_id(Param::LinkState), false, &[0x01, 0x00]),
            response(param_id(Param::Battery), false, &[49]),
            response(param_id(Param::Sidetone), false, &[7]),
            response(param_id(Param::GameChatBalance), false, &[10]),
            response(param_id(Param::MicMute), false, &[1]),
            response(param_id(Param::NoiseCancellation), false, &[0x01, 0x03]),
        ] {
            b.push_read(&info.id, r);
        }

        let states = run_to_completion(&b);
        let last = states.last().unwrap();
        assert_eq!(last.connected, Some(true));
        assert_eq!(last.battery, Some(49));
        assert_eq!(last.sidetone, Some(7));
        assert_eq!(last.game_chat, Some(10));
        assert_eq!(last.mic_mute_hardware, Some(true));
        assert_eq!(last.effectively_muted(), Some(true));
        assert_eq!(
            last.noise,
            Some(NoiseControl {
                mode: NoiseMode::Anc,
                anc_level: 3
            })
        );
    }

    #[test]
    fn setting_noise_writes_both_bytes_and_reads_back() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        // Link read short-circuits the rest of the refresh, so the only
        // exchanges after it are the write and its read-back.
        for r in [
            response(param_id(Param::LinkState), false, &[0x00, 0x00]),
            response(param_id(Param::NoiseCancellation), true, &[0x00]),
            response(param_id(Param::NoiseCancellation), false, &[0x50, 0x03]),
        ] {
            b.push_read(&info.id, r);
        }

        let (tx, rx) = mpsc::channel();
        tx.send(Command::SetNoise(NoiseControl {
            mode: NoiseMode::Ambient,
            anc_level: 3,
        }))
        .unwrap();
        tx.send(Command::Shutdown).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        run(&b, rx, move |s| sink.lock().unwrap().push(s));
        let states = seen.lock().unwrap().clone();

        let writes = b.writes();
        let cmd = writes
            .iter()
            .find(|w| w[10] == 0x92)
            .expect("a noise write must reach the wire");
        assert_eq!(cmd[12], 2, "both bytes go out together");
        // Ambient carries the byte the vendor software was observed sending,
        // not the retained level.
        assert_eq!(&cmd[13..15], &[0x50, 0x01]);

        assert_eq!(
            states.last().unwrap().noise,
            Some(NoiseControl {
                mode: NoiseMode::Ambient,
                anc_level: 3
            }),
            "the state must come from the read-back, not from what was asked"
        );
    }

    #[test]
    fn a_refused_read_does_not_reach_the_screen_as_255() {
        let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let info = resolve_control_device(&b).unwrap();
        for r in [
            response(param_id(Param::LinkState), false, &[0x01, 0x00]),
            response(param_id(Param::Battery), false, &[0xFF]),
            response(param_id(Param::Sidetone), false, &[0xFF]),
            response(param_id(Param::GameChatBalance), false, &[0xFF]),
            response(param_id(Param::MicMute), false, &[0xFF]),
            response(param_id(Param::NoiseCancellation), false, &[0xFF]),
        ] {
            b.push_read(&info.id, r);
        }

        let states = run_to_completion(&b);
        let last = states.last().unwrap();
        assert_eq!(last.battery, None);
        assert_eq!(last.sidetone, None);
        assert_eq!(last.noise, None);
        assert!(!last.tooltip().contains("255"), "{}", last.tooltip());
    }

    mod v2_pro {
        use super::*;

        const FIXTURE: &str =
            include_str!("../../headset-device/tests/fixtures/blackshark-v2-pro.json");

        /// One report holding one family-08 message, shaped as the V2 Pro
        /// was observed shaping them.
        fn report(cmd: u8, role: u8, value: &[u8]) -> Vec<u8> {
            let mut msg = vec![0x50, 0x49, 0x08, 0xC0, 0x11, 0x22, 0x33, 0x00];
            msg.push(3 + value.len() as u8);
            msg.push(0x00);
            msg.extend([cmd, role, value.len() as u8]);
            msg.extend_from_slice(value);
            let mut r = vec![0u8; CONTROL_REPORT_LEN];
            r[0] = 0x02;
            r[1] = msg.len() as u8;
            r[2..2 + msg.len()].copy_from_slice(&msg);
            r
        }

        fn run_commands(b: &FakeHidBackend, cmds: Vec<Command>) -> Vec<HeadsetState> {
            let (tx, rx) = mpsc::channel();
            for c in cmds {
                tx.send(c).unwrap();
            }
            tx.send(Command::Shutdown).unwrap();
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = seen.clone();
            run(b, rx, move |s| sink.lock().unwrap().push(s));
            let out = seen.lock().unwrap().clone();
            out
        }

        #[test]
        fn a_refresh_reads_only_what_this_device_answers() {
            let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
            let info = resolve_control_device(&b).unwrap();
            b.push_read(&info.id, report(0x20, 0x01, &[0x01]));
            b.push_read(&info.id, report(0x21, 0x01, &[0x37]));

            let states = run_to_completion(&b);
            let last = states.last().unwrap();
            assert_eq!(last.connected, Some(true));
            assert_eq!(last.battery, Some(55));
            assert_eq!(last.model, Some("BlackShark V2 Pro"));
            assert!(!last.features.game_chat && !last.features.noise);
            // The opening e1 request (S26), then link and battery only:
            // sidetone was never read on this device, mic mute only ever
            // pushed, and game/chat and noise do not exist.
            let reads: Vec<u8> = b.writes().iter().map(|w| w[10]).collect();
            assert_eq!(reads, vec![0xE1, 0x20, 0x21]);
        }

        #[test]
        fn setting_sidetone_sends_the_preamble_and_marks_the_level_unconfirmed() {
            let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
            let info = resolve_control_device(&b).unwrap();
            for r in [
                // The startup refresh, cut short by a link that is down.
                report(0x20, 0x01, &[0x00]),
                report(0x98, 0x01, &[0x00]),
                report(0x99, 0x01, &[0x00]),
            ] {
                b.push_read(&info.id, r);
            }

            let states = run_commands(&b, vec![Command::SetSidetone(13)]);
            let cmds: Vec<u8> = b.writes().iter().map(|w| w[10]).collect();
            assert_eq!(
                cmds,
                vec![0xE1, 0x20, 0x98, 0x99],
                "no read-back is invented"
            );
            let last = states.last().unwrap();
            assert_eq!(last.sidetone, Some(13));
            assert!(last.sidetone_unconfirmed);
        }

        #[test]
        fn silence_reads_as_a_headset_that_is_off_not_a_wedged_dongle() {
            // Nothing canned: every read goes unanswered, which is what this
            // dongle does while the headset is switched off.
            let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
            let states = run_to_completion(&b);
            let last = states.last().unwrap();
            assert_eq!(last.connected, Some(false));
            assert!(
                !last.dongle_silent,
                "no replug advice for a headset that is off"
            );
        }

        #[test]
        fn a_mute_event_from_the_headset_is_understood() {
            let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
            let info = resolve_control_device(&b).unwrap();
            let mut s = HeadsetState::default();
            b.push_read(&info.id, report(0x55, 0x02, &[0x01]));
            let mut session = ControlSession::open_read_only(&b).unwrap();
            for e in session.listen(Duration::from_millis(300)).unwrap() {
                s.apply(session.codec(), &e);
            }
            assert_eq!(s.mic_mute_hardware, Some(true));
        }
    }
}
