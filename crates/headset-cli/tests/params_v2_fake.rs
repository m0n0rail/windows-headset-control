//! `get` / `set` / `param` / `watch` against the BlackShark V2 Pro fixture.
//! No hardware, no writes to a real device.
//!
//! The reports here are shaped the way the V2 Pro was observed shaping them:
//! `02 LEN` and one or more `50 49 08 …` messages. See
//! `docs/device-research-blackshark-v2-pro.md`.

use headset_cli::cli::{GetArgs, NoiseArgs, ParamAction, ParamArgs, SetArgs, WatchArgs};
use headset_cli::cmd::params;
use headset_device::{resolve_control_device, FakeHidBackend};
use headset_protocol::CONTROL_REPORT_LEN;

const FIXTURE: &str = include_str!("../../headset-device/tests/fixtures/blackshark-v2-pro.json");

const RESPONSE: u8 = 0x01;
const EVENT: u8 = 0x02;

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

fn backend_with(reports: Vec<Vec<u8>>) -> FakeHidBackend {
    let mut b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
    let info = resolve_control_device(&b).unwrap();
    for r in reports {
        b.push_read(&info.id, r);
    }
    b
}

fn get(b: &FakeHidBackend, name: &str) -> anyhow::Result<String> {
    params::run_get(b, &GetArgs { name: name.into() }, false)
}

fn set(b: &FakeHidBackend, name: &str, value: u8) -> anyhow::Result<String> {
    params::run_set(
        b,
        &SetArgs {
            name: name.into(),
            value,
        },
        false,
    )
}

/// The command byte of every write that reached the wire, in order.
fn commands(b: &FakeHidBackend) -> Vec<u8> {
    b.writes().iter().map(|w| w[10]).collect()
}

#[test]
fn battery_reads_as_a_percentage() {
    let b = backend_with(vec![report(0x21, RESPONSE, &[0x37])]);
    assert_eq!(get(&b, "battery").unwrap(), "battery: 55\n");
}

#[test]
fn a_one_byte_link_state_reads_as_words() {
    let b = backend_with(vec![report(0x20, RESPONSE, &[0x01])]);
    assert_eq!(get(&b, "link").unwrap(), "link: connected\n");
    let b = backend_with(vec![report(0x20, RESPONSE, &[0x00])]);
    assert_eq!(get(&b, "link").unwrap(), "link: not connected\n");
}

#[test]
fn a_parameter_only_ever_written_is_not_read() {
    let b = backend_with(vec![]);
    let err = get(&b, "sidetone").unwrap_err().to_string();
    assert!(err.contains("never observed being read"), "{err}");
    assert!(err.contains("only ever wrote it"), "{err}");
    assert!(b.writes().is_empty(), "no read invented for it");
}

#[test]
fn a_parameter_only_ever_pushed_is_not_read() {
    let b = backend_with(vec![]);
    let err = get(&b, "mic-mute").unwrap_err().to_string();
    assert!(err.contains("as an event"), "{err}");
    assert!(b.writes().is_empty());
}

#[test]
fn a_parameter_this_device_does_not_have_is_named_as_such() {
    let b = backend_with(vec![]);
    let err = get(&b, "game-chat").unwrap_err().to_string();
    assert!(
        err.contains("not established on the BlackShark V2 Pro"),
        "{err}"
    );
    let err = params::run_noise(
        &b,
        &NoiseArgs {
            mode: None,
            level: None,
        },
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("not established"), "{err}");
    assert!(b.writes().is_empty());
}

#[test]
fn sidetone_sends_the_observed_preamble_and_says_it_cannot_confirm() {
    let b = backend_with(vec![
        report(0x98, RESPONSE, &[0x00]),
        report(0x99, RESPONSE, &[0x00]),
    ]);
    let out = set(&b, "sidetone", 13).unwrap();
    assert_eq!(commands(&b), vec![0x98, 0x99], "S04: 98 01 before every 99");
    assert_eq!(b.writes()[0][13], 0x01);
    assert_eq!(b.writes()[1][13], 13);
    assert!(out.contains("unconfirmed"), "{out}");
    assert!(
        !out.contains("sidetone: 13\n"),
        "must not read as verified: {out}"
    );
}

#[test]
fn sidetone_json_says_the_value_is_not_verifiable() {
    let b = backend_with(vec![
        report(0x98, RESPONSE, &[0x00]),
        report(0x99, RESPONSE, &[0x00]),
    ]);
    let out = params::run_set(
        &b,
        &SetArgs {
            name: "sidetone".into(),
            value: 7,
        },
        true,
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["verifiable"], false);
    assert!(v["readback"].is_null());
    assert!(v["applied"].is_null());
}

#[test]
fn power_saving_is_written_and_read_back() {
    let b = backend_with(vec![
        report(0xAC, RESPONSE, &[0x00]),
        report(0x2C, RESPONSE, &[30]),
    ]);
    assert_eq!(set(&b, "power-saving", 30).unwrap(), "power-saving: 30\n");
    assert_eq!(commands(&b), vec![0xAC, 0x2C]);
}

#[test]
fn power_saving_refuses_a_timeout_the_slider_never_produced() {
    for value in [1u8, 14, 61, 255] {
        let b = backend_with(vec![]);
        let err = set(&b, "power-saving", value).unwrap_err().to_string();
        assert!(err.contains("never observed taking"), "{value}: {err}");
        assert!(b.writes().is_empty(), "{value} must not be sent");
    }
}

#[test]
fn do_not_disturb_round_trips() {
    let b = backend_with(vec![
        report(0xA7, RESPONSE, &[0x00]),
        report(0x27, RESPONSE, &[0x01]),
    ]);
    assert_eq!(set(&b, "do-not-disturb", 1).unwrap(), "do-not-disturb: 1\n");
}

#[test]
fn mic_eq_accepts_its_ack_of_01_as_success() {
    // S16: every 0x96 write was acknowledged with 01, and took effect.
    let b = backend_with(vec![report(0x96, RESPONSE, &[0x01])]);
    let out = set(&b, "mic-eq", 2).unwrap();
    assert!(out.contains("unconfirmed"), "{out}");
    let b = backend_with(vec![]);
    assert!(set(&b, "mic-eq", 4).is_err(), "only 0-3 and 0xff were seen");
}

#[test]
fn an_unrecognised_ack_is_not_success() {
    let b = backend_with(vec![report(0xA7, RESPONSE, &[0x01])]);
    let err = set(&b, "do-not-disturb", 1).unwrap_err().to_string();
    assert!(err.contains("unrecognised result 0x01"), "{err}");
}

#[test]
fn the_eq_preset_cannot_be_written_on_its_own() {
    let b = backend_with(vec![]);
    let err = set(&b, "eq-preset", 0x07).unwrap_err().to_string();
    assert!(err.contains("longer sequence"), "{err}");
    assert!(b.writes().is_empty());
}

#[test]
fn the_eq_preset_can_be_read() {
    let b = backend_with(vec![report(0x13, RESPONSE, &[0x07])]);
    assert_eq!(get(&b, "eq-preset").unwrap(), "eq-preset: 7\n");
}

#[test]
fn the_serial_parameter_is_refused() {
    let b = backend_with(vec![]);
    let args = ParamArgs {
        action: ParamAction::Get {
            id: 0x00,
            index: None,
        },
    };
    let err = params::run_param(&b, &args, false).unwrap_err().to_string();
    assert!(err.contains("not in the observed read allowlist"), "{err}");
    assert!(b.writes().is_empty());
}

#[test]
fn raw_reads_reach_unidentified_parameters() {
    let b = backend_with(vec![report(0x2A, RESPONSE, &[0x00])]);
    let args = ParamArgs {
        action: ParamAction::Get {
            id: 0x2A,
            index: None,
        },
    };
    assert_eq!(params::run_param(&b, &args, false).unwrap(), "0x2a: 00\n");
}

#[test]
fn watch_names_the_events_this_device_pushes() {
    let b = backend_with(vec![
        report(0x55, EVENT, &[0x01]),
        report(0x13, EVENT, &[0x07]),
        report(0x21, EVENT, &[0x35]),
    ]);
    let out = params::run_watch(&b, &WatchArgs { seconds: 1 }, false).unwrap();
    assert!(out.contains("mic-mute"), "{out}");
    assert!(out.contains("eq-preset"), "{out}");
    assert!(out.contains("battery"), "{out}");
    assert!(b.writes().is_empty(), "watch sends nothing");
}
