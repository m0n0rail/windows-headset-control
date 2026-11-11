//! `get`, `set`, `param`, and `watch`.
//!
//! Everything here goes through `ControlSession`, so device resolution and
//! request/response correlation are solved once, below this layer.

use std::time::Duration;

use anyhow::{bail, Result};
use headset_device::{ControlSession, HidBackend};
use headset_protocol::v3pro::noise::{
    noise_write_payload, NoiseControl, NoiseMode, ANC_LEVEL_RANGE,
};
use headset_protocol::{Codec, Param, ParamFrame, ResultCode};
use serde_json::json;

use crate::cli::{GetArgs, NoiseArgs, NoiseModeArg, ParamAction, ParamArgs, SetArgs, WatchArgs};
use crate::render::json::SCHEMA_VERSION;

fn hex_list(ids: &[u8]) -> String {
    ids.iter()
        .map(|i| format!("{i:#04x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn lookup(name: &str) -> Result<Param> {
    Param::from_name(name).ok_or_else(|| {
        let names = Param::ALL
            .iter()
            .map(|p| p.name())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("unknown parameter `{name}`; known parameters are: {names}")
    })
}

/// This device's identifier for `param`, or an error naming the device when it
/// has no established equivalent.
fn id_on(session: &ControlSession, param: Param) -> Result<u8> {
    session.codec().id(param).ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` is not established on the {}; there is no identifier to send",
            param.name(),
            session.profile().name
        )
    })
}

/// Reads the link state to explain a refusal. `None` when it cannot be read.
fn probe_link(session: &mut ControlSession) -> Option<bool> {
    let codec = session.codec();
    let id = codec.id(Param::LinkState)?;
    let probe = codec.encode_read(id, None).ok()?;
    session
        .exchange(&probe, id, false)
        .ok()
        .map(|f| link_is_up(&f))
}

/// Whether a read came back refused, by the device's own observed convention.
///
/// Only applied to named parameters. The raw `param get` path prints whatever
/// byte arrived, because an unidentified parameter might legitimately hold the
/// same byte and this project does not guess.
fn looks_refused(codec: &dyn Codec, param: Param, frame: &ParamFrame) -> bool {
    codec.read_refused(param, frame)
}

/// `true` when the headset is reachable over the wireless link.
fn link_is_up(frame: &ParamFrame) -> bool {
    frame.payload.first() == Some(&0x01)
}

pub fn run_get(backend: &dyn HidBackend, args: &GetArgs, as_json: bool) -> Result<String> {
    let param = lookup(&args.name)?;
    let mut session = ControlSession::open(backend)?;
    let id = id_on(&session, param)?;
    if !session.codec().is_readable(param) {
        bail!(
            "`{}` was never observed being read on the {}: the vendor software only \
             ever {} it, so there is no read to send",
            param.name(),
            session.profile().name,
            if session.codec().is_writable(param) {
                "wrote"
            } else {
                "received it as an event from"
            }
        );
    }
    let request = session.codec().encode_read(id, None)?;
    let frame = session.exchange(&request, id, false)?;

    // Only pay for the extra exchange when something is actually wrong: a
    // healthy read costs one round trip, and the link is consulted purely to
    // explain a refusal.
    let link_up = if looks_refused(session.codec(), param, &frame) {
        probe_link(&mut session)
    } else {
        None
    };

    let refused = looks_refused(session.codec(), param, &frame);

    Ok(if as_json {
        serde_json::to_string_pretty(&json!({
            "schema_version": SCHEMA_VERSION,
            "parameter": param.name(),
            "id": format!("{id:#04x}"),
            // Null rather than 255 when refused: 255 is not a battery level,
            // and emitting it would put a wrong number into a machine-readable
            // contract.
            "value": if refused { None } else { frame.value() },
            "available": !refused,
            "headset_connected": link_up,
            "payload_hex": frame.hex_payload(),
            "wrote_to_device": true,
        }))
        .expect("serialization cannot fail")
    } else if refused {
        match link_up {
            Some(false) => format!(
                "{}: unavailable - the headset is powered off or out of range\n",
                param.name()
            ),
            _ => format!(
                "{}: unavailable - the device returned the refusal byte 0xff\n",
                param.name()
            ),
        }
    } else {
        match frame.value() {
            // Link state is words, whatever its width on the device.
            _ if param == Param::LinkState => format!(
                "{}: {}\n",
                param.name(),
                if link_is_up(&frame) {
                    "connected"
                } else {
                    "not connected"
                }
            ),
            Some(v) => format!("{}: {v}\n", param.name()),
            None if param == Param::NoiseCancellation => {
                match NoiseControl::from_payload(&frame.payload) {
                    Some(c) => format!("{}: {}\n", param.name(), c.describe()),
                    None => format!("{}: {}\n", param.name(), frame.hex_payload()),
                }
            }
            None => format!("{}: {}\n", param.name(), frame.hex_payload()),
        }
    })
}

pub fn run_set(backend: &dyn HidBackend, args: &SetArgs, as_json: bool) -> Result<String> {
    let param = lookup(&args.name)?;
    if param == Param::NoiseCancellation {
        bail!(
            "`noise-cancellation` holds a mode and a level in two bytes that the device was \
             only ever seen written together, so a single value cannot express it. Use \
             `headsetctl noise --mode <off|anc|ambient> --level <1-4>`."
        );
    }

    // Whether a parameter is writable, and over what range, is what was
    // observed on a particular device, so the device is identified first.
    // Nothing is sent until every check below has passed.
    let mut session = ControlSession::open(backend)?;
    let codec = session.codec();
    let id = id_on(&session, param)?;
    if !codec.is_writable(param) {
        bail!(
            "`{}` is read-only: no write for it was ever observed on the wire, so this \
             project has no command to send",
            param.name()
        );
    }
    if !codec.standalone_write(param) {
        bail!(
            "`{}` was only ever written as one step of a longer sequence on the {}, \
             so writing it on its own would send something never observed",
            param.name(),
            session.profile().name
        );
    }
    if !codec.accepts(param, args.value) {
        bail!(
            "{} was never observed taking {} on this hardware{}",
            param.name(),
            args.value,
            codec
                .range(param)
                .map(|(lo, hi)| format!("; it accepts {lo}..={hi}"))
                .unwrap_or_default()
        );
    }

    if let Some((pre_id, pre_value)) = codec.write_preamble(param) {
        let pre = codec.encode_write(pre_id, &[pre_value])?;
        let ack = session.exchange(&pre, pre_id, true)?;
        check_result(codec, &ack, &format!("{} enable", param.name()))?;
    }

    let request = codec.encode_write(id, &[args.value])?;
    let ack = session.exchange(&request, id, true)?;
    check_result(codec, &ack, param.name())?;

    // Device is the source of truth: a successful write is not evidence that
    // the value changed. Read it back and report what the device actually holds
    // -- where a read was ever observed. Where none was, say so rather than
    // inventing one or reporting the requested value as confirmed.
    if !codec.is_readable(param) {
        return Ok(if as_json {
            serde_json::to_string_pretty(&json!({
                "schema_version": SCHEMA_VERSION,
                "parameter": param.name(),
                "id": format!("{id:#04x}"),
                "requested": args.value,
                "readback": serde_json::Value::Null,
                "applied": serde_json::Value::Null,
                "verifiable": false,
                "wrote_to_device": true,
            }))
            .expect("serialization cannot fail")
        } else {
            format!(
                "{}: wrote {}; acknowledged, but the {} was never observed answering a read \
                 of it, so the value is unconfirmed\n",
                param.name(),
                args.value,
                session.profile().name
            )
        });
    }
    let verify = codec.encode_read(id, None)?;
    let now = session.exchange(&verify, id, false)?;
    let readback = now.value();

    Ok(if as_json {
        serde_json::to_string_pretty(&json!({
            "schema_version": SCHEMA_VERSION,
            "parameter": param.name(),
            "id": format!("{id:#04x}"),
            "requested": args.value,
            "readback": readback,
            "applied": readback == Some(args.value),
            "verifiable": true,
            "wrote_to_device": true,
        }))
        .expect("serialization cannot fail")
    } else {
        match readback {
            Some(v) if v == args.value => format!("{}: {v}\n", param.name()),
            Some(v) => format!(
                "{}: wrote {} but the device reports {v}\n",
                param.name(),
                args.value
            ),
            None => format!(
                "{}: wrote {}; read-back returned {}\n",
                param.name(),
                args.value,
                now.hex_payload()
            ),
        }
    })
}

fn mode_of(arg: NoiseModeArg) -> NoiseMode {
    match arg {
        NoiseModeArg::Off => NoiseMode::Off,
        NoiseModeArg::Anc => NoiseMode::Anc,
        NoiseModeArg::Ambient => NoiseMode::Ambient,
    }
}

/// Show or change noise cancellation.
///
/// Mode and level live in one two-byte parameter, so every change is a
/// read-modify-write: whichever field the caller left out is taken from the
/// device and sent back unchanged. That is what the vendor software was
/// observed doing, and writing one byte without the other would clobber it.
pub fn run_noise(backend: &dyn HidBackend, args: &NoiseArgs, as_json: bool) -> Result<String> {
    // Checked before the device is opened: a level never seen on the wire must
    // not be sent, and nothing about the current state changes that.
    let (lo, hi) = ANC_LEVEL_RANGE;
    if let Some(level) = args.level {
        if level < lo || level > hi {
            bail!(
                "anc level accepts {lo}..={hi}; {level} was never observed on this hardware, \
                 so there is no evidence for what the device would do with it"
            );
        }
    }

    let mut session = ControlSession::open(backend)?;
    let codec = session.codec();
    let id = id_on(&session, Param::NoiseCancellation)?;
    let read = codec.encode_read(id, None)?;
    let frame = session.exchange(&read, id, false)?;

    let Some(current) = NoiseControl::from_payload(&frame.payload) else {
        // Same refusal the other proxied parameters give when the headset is
        // unreachable. Consult the link only to explain it.
        let link_up = probe_link(&mut session);
        return Ok(render_noise(id, &frame, None, link_up, None, as_json));
    };

    if args.mode.is_none() && args.level.is_none() {
        return Ok(render_noise(id, &frame, Some(current), None, None, as_json));
    }

    let desired = NoiseControl {
        mode: args.mode.map_or(current.mode, mode_of),
        anc_level: args.level.unwrap_or(current.anc_level),
    };
    let request = codec.encode_write(id, &noise_write_payload(desired)?)?;
    let ack = session.exchange(&request, id, true)?;
    check_result(codec, &ack, "noise-cancellation")?;

    // Device is the source of truth: a successful write is not evidence that
    // the value changed.
    let verify = codec.encode_read(id, None)?;
    let now = session.exchange(&verify, id, false)?;
    let readback = NoiseControl::from_payload(&now.payload);

    Ok(render_noise(
        id,
        &now,
        readback,
        None,
        Some(desired),
        as_json,
    ))
}

/// Renders a noise-control state, whether it was just read or just written.
///
/// `requested` is present only after a write, and is what makes the difference
/// between "this is the state" and "this is the state despite what was asked".
fn render_noise(
    id: u8,
    frame: &ParamFrame,
    state: Option<NoiseControl>,
    link_up: Option<bool>,
    requested: Option<NoiseControl>,
    as_json: bool,
) -> String {
    let name = Param::NoiseCancellation.name();
    if as_json {
        return serde_json::to_string_pretty(&json!({
            "schema_version": SCHEMA_VERSION,
            "parameter": name,
            "id": format!("{id:#04x}"),
            // Null rather than a guess: an unrecognised mode byte has no name,
            // and a refused read has no state at all.
            "mode": state.and_then(|s| s.mode.name()),
            "mode_byte": state.map(|s| format!("{:#04x}", s.mode.to_byte())),
            "anc_level": state.map(|s| s.anc_level),
            "available": state.is_some(),
            "headset_connected": link_up,
            "requested_mode": requested.and_then(|r| r.mode.name()),
            "requested_anc_level": requested.map(|r| r.anc_level),
            "applied": requested.map(|r| state == Some(r)),
            "payload_hex": frame.hex_payload(),
            "wrote_to_device": true,
        }))
        .expect("serialization cannot fail");
    }

    match (state, requested) {
        (None, _) => match link_up {
            Some(false) => {
                format!("{name}: unavailable - the headset is powered off or out of range\n")
            }
            _ => format!(
                "{name}: unavailable - the device answered with {}\n",
                frame.hex_payload()
            ),
        },
        (Some(now), Some(asked)) if now != asked => format!(
            "{name}: wrote {} but the device reports {}\n",
            asked.describe(),
            now.describe()
        ),
        (Some(now), _) => format!("{name}: {}\n", now.describe()),
    }
}

fn check_result(codec: &dyn Codec, ack: &ParamFrame, what: &str) -> Result<()> {
    match codec.write_result(ack) {
        Some(ResultCode::Ok) | None => Ok(()),
        Some(ResultCode::Refused) => bail!(
            "the device refused the {what} write (result 0xff). This was observed when the \
             microphone is muted by the headset's own switch; unmuting it and retrying is \
             the first thing to try."
        ),
        Some(ResultCode::Unknown(b)) => bail!(
            "the device answered the {what} write with an unrecognised result {b:#04x}; \
             only 0x00 and 0xff have been observed, so this is not being treated as success"
        ),
    }
}

pub fn run_param(backend: &dyn HidBackend, args: &ParamArgs, as_json: bool) -> Result<String> {
    match &args.action {
        ParamAction::Get { id, index } => {
            // The allowlists are per device, so the device is identified before
            // the identifier is checked. Nothing is sent until it has passed.
            let mut session = ControlSession::open(backend)?;
            let codec = session.codec();
            if !codec.read_allowlist().contains(id) {
                bail!(
                    "{id:#04x} is not in the observed read allowlist. Only identifiers seen \
                     on the wire may be sent. Allowed: {}",
                    hex_list(codec.read_allowlist())
                );
            }
            if index.is_some() && !codec.indexed_reads().contains(id) {
                bail!(
                    "{id:#04x} was not observed taking an index; indexed reads are: {}",
                    hex_list(codec.indexed_reads())
                );
            }
            let request = codec.encode_read(*id, *index)?;
            let frame = session.exchange(&request, *id, false)?;
            Ok(render_raw(*id, false, &frame, as_json))
        }
        ParamAction::Set { id, value } => {
            let mut session = ControlSession::open(backend)?;
            let codec = session.codec();
            if !codec.write_allowlist().contains(id) {
                bail!(
                    "{id:#04x} is not in the observed write allowlist. Only identifiers seen \
                     being written may be sent. Allowed: {}",
                    hex_list(codec.write_allowlist())
                );
            }
            if codec.write_payload_len(*id) != 1 {
                bail!(
                    "{id:#04x} was only ever observed being written with {} payload bytes, \
                     which `param set` cannot express. Use `headsetctl noise` for it.",
                    match codec.write_payload_len(*id) {
                        2 => "two".to_string(),
                        n => n.to_string(),
                    }
                );
            }
            let request = codec.encode_write(*id, &[*value])?;
            let frame = session.exchange(&request, *id, true)?;
            Ok(render_raw(*id, true, &frame, as_json))
        }
    }
}

fn render_raw(id: u8, is_write: bool, frame: &ParamFrame, as_json: bool) -> String {
    if as_json {
        serde_json::to_string_pretty(&json!({
            "schema_version": SCHEMA_VERSION,
            "id": format!("{id:#04x}"),
            "write": is_write,
            "payload_hex": frame.hex_payload(),
            "value": frame.value(),
            // Deliberately absent: any interpretation of these bytes. Eleven
            // observed parameters have no established meaning, and inventing
            // one here would put it into a machine-readable contract.
            "interpretation": serde_json::Value::Null,
            "wrote_to_device": true,
        }))
        .expect("serialization cannot fail")
    } else {
        format!("{id:#04x}: {}\n", frame.hex_payload())
    }
}

pub fn run_watch(backend: &dyn HidBackend, args: &WatchArgs, as_json: bool) -> Result<String> {
    let mut session = ControlSession::open_read_only(backend)?;
    let events = session.listen(Duration::from_secs(args.seconds.clamp(1, 3600)))?;

    Ok(if as_json {
        let items: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                json!({
                    "id": format!("{:#04x}", e.param),
                    "name": name_of(session.codec(), e.param),
                    "value": e.value(),
                    "payload_hex": e.hex_payload(),
                })
            })
            .collect();
        serde_json::to_string_pretty(&json!({
            "schema_version": SCHEMA_VERSION,
            "seconds": args.seconds,
            "events": items,
            "wrote_to_device": false,
        }))
        .expect("serialization cannot fail")
    } else if events.is_empty() {
        "no events. The device pushes only when something changes - move the onboard \
         wheel or toggle the mic mute to produce one.\n"
            .to_string()
    } else {
        let mut s = String::new();
        for e in &events {
            use std::fmt::Write as _;
            let label = name_of(session.codec(), e.param).unwrap_or("unidentified");
            let _ = writeln!(
                s,
                "{:#04x} {:<16} {}",
                e.param,
                label,
                e.value().map_or_else(|| e.hex_payload(), |v| v.to_string())
            );
        }
        s
    })
}

/// The name of a parameter, or `None` when its meaning is not established.
fn name_of(codec: &dyn Codec, id: u8) -> Option<&'static str> {
    codec.param_for(id).map(|p| p.name())
}
