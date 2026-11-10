//! The tray's view of the headset. Safe Rust; no OS access.
//!
//! Nothing here is cached authoritatively. Every field is `Option`, and
//! `Unknown` is a first-class state rather than a zero standing in for one.
//! That is the whole point of the device-is-source-of-truth rule: a value we
//! have not read, or that the device refused, must not render as a number.

use headset_protocol::v3pro::noise::NoiseControl;
use headset_protocol::{Codec, Param, ParamFrame};

/// The refusal byte. A named parameter answering with it means "unavailable",
/// not the value 255.
const REFUSED: u8 = 0xFF;

/// Shown before the device has been identified, and if its product string is
/// missing. Deliberately generic rather than a guessed model number.
const FALLBACK_NAME: &str = "Headset";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadsetState {
    /// The HID product string, as reported by the device. Not a hardcoded model
    /// name: the mockups say "V2 Pro" but the hardware reports what it reports.
    pub device_name: Option<String>,
    /// `None` until the link parameter has been read at least once.
    pub connected: Option<bool>,
    pub battery: Option<u8>,
    pub sidetone: Option<u8>,
    pub game_chat: Option<u8>,
    /// Noise-control mode and ANC level, which the device holds as one
    /// two-byte parameter and which are therefore never separately known.
    pub noise: Option<NoiseControl>,
    /// The headset's hardware mute switch.
    pub mic_mute_hardware: Option<bool>,
    /// The Windows capture endpoint's mute, which is a separate state.
    pub mic_mute_os: Option<bool>,
    /// Set when the control channel opened but the dongle answered nothing.
    ///
    /// Distinct from `connected: None`, which says only that the link state has
    /// not been read yet. `0x20` is answered by the dongle itself and returns a
    /// value even with the headset powered off, so silence there is positive
    /// evidence that the dongle has stopped talking rather than that the
    /// headset is away. The remedy is a replug, and the tray has to say so: it
    /// rendered this as SEARCHING indefinitely, which is the same thing it
    /// shows while starting up and told the user nothing.
    pub dongle_silent: bool,
    /// Whether to warn that Razer's engine is running and may contend for
    /// settings. This is detection **and** the user's preference combined: the
    /// warning is suppressed when they have turned it off, so a single flag is
    /// what the renderers need. Raw detection lives in `win32::process_running`.
    pub warn_vendor_software: bool,
}

impl HeadsetState {
    /// Copies the fields the device thread owns, leaving the rest alone.
    ///
    /// State has two owners and they must not overwrite each other. The worker
    /// owns everything read from the headset; the UI thread owns
    /// `mic_mute_os` (a Windows audio endpoint, which the worker never reads)
    /// and `warn_vendor_software` (a user setting combined with a process
    /// check). Replacing the whole struct with the worker's snapshot silently
    /// reverted both — turning the Synapse warning off and then refreshing
    /// brought the warning straight back, because the worker's copy still said
    /// it was on.
    pub fn apply_device_snapshot(&mut self, from: &HeadsetState) {
        self.device_name = from.device_name.clone();
        self.connected = from.connected;
        self.battery = from.battery;
        self.sidetone = from.sidetone;
        self.game_chat = from.game_chat;
        self.noise = from.noise;
        self.mic_mute_hardware = from.mic_mute_hardware;
        self.dongle_silent = from.dongle_silent;
    }

    /// This state as the panel should draw it while a noise write is in flight.
    ///
    /// The device is still the source of truth — this does not touch `self`,
    /// and the read-back that follows every write is what finally decides. It
    /// exists because a write costs at least one 250 ms-paced exchange, and a
    /// control that does not move when clicked reads as broken.
    ///
    /// A pending write against an unknown state is ignored: there is nothing to
    /// preview against, and the device may refuse the write outright.
    pub fn with_pending_noise(&self, pending: Option<NoiseControl>) -> HeadsetState {
        let mut out = self.clone();
        if self.noise.is_some() {
            if let Some(p) = pending {
                out.noise = Some(p);
            }
        }
        out
    }

    /// Display name for the header.
    ///
    /// Strips the trailing " HID" the descriptor carries, which is an artefact
    /// of the interface rather than part of the product's name.
    pub fn device_name(&self) -> String {
        match &self.device_name {
            Some(n) => n.trim().trim_end_matches(" HID").trim().to_string(),
            None => FALLBACK_NAME.to_string(),
        }
    }

    /// Audio is silenced if either mute is set, so the tray reports the union.
    /// Reporting only one of them would tell the user their mic is live when it
    /// is not.
    pub fn effectively_muted(&self) -> Option<bool> {
        match (self.mic_mute_hardware, self.mic_mute_os) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(false) || b.unwrap_or(false)),
        }
    }

    /// Folds one decoded frame into the state.
    ///
    /// Applies to responses and events alike: both carry the same parameter and
    /// payload, and the tray does not care which prompted the update. `codec`
    /// is the protocol of the device the frame came from, which is what says
    /// which setting its identifier stands for.
    pub fn apply(&mut self, codec: &dyn Codec, frame: &ParamFrame) {
        let value = frame.value().filter(|v| *v != REFUSED);
        match codec.param_for(frame.param) {
            Some(Param::LinkState) => {
                self.connected = Some(frame.payload.first() == Some(&0x01));
                // A dropped link invalidates everything proxied through it.
                // Leaving stale numbers on screen would be worse than a gap.
                if self.connected == Some(false) {
                    self.battery = None;
                    self.sidetone = None;
                    self.game_chat = None;
                    self.noise = None;
                    self.mic_mute_hardware = None;
                }
            }
            Some(Param::Battery) => self.battery = value,
            Some(Param::Sidetone) => self.sidetone = value,
            Some(Param::GameChatBalance) => self.game_chat = value,
            Some(Param::MicMute) => self.mic_mute_hardware = value.map(|v| v != 0),
            // Both bytes or nothing: a refusal is one byte and decodes to
            // `None`, which is the same "unknown" every other field uses.
            Some(Param::NoiseCancellation) => {
                self.noise = NoiseControl::from_payload(&frame.payload)
            }
            // Parameters with no established meaning are ignored rather than
            // guessed at. The tray shows only what the research record supports.
            Some(Param::SliderFunction) | None => {}
        }
    }

    /// Tooltip text. Kept under the 127-character limit Windows imposes on
    /// `NOTIFYICONDATAW::szTip`.
    pub fn tooltip(&self) -> String {
        let mut s = String::from("BlackShark V3 Pro");
        if self.dongle_silent {
            s.push_str(" - dongle not responding; unplug it and plug it back in");
            return s;
        }
        match (self.connected, self.battery) {
            (Some(false), _) => s.push_str(" - off"),
            (_, Some(b)) => s.push_str(&format!(" - battery {b}%")),
            (_, None) => s.push_str(" - battery unknown"),
        }
        if self.effectively_muted() == Some(true) {
            s.push_str(" - mic muted");
        }
        if self.warn_vendor_software {
            s.push_str("\nSynapse is running and may change settings");
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use headset_protocol::v3pro::{param_id, V3_PRO};
    use headset_protocol::Role;

    fn frame(param: u8, payload: &[u8]) -> ParamFrame {
        ParamFrame {
            param,
            is_write: false,
            role: Role::Response,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn a_silent_dongle_tooltip_names_the_remedy() {
        let s = HeadsetState {
            dongle_silent: true,
            ..HeadsetState::default()
        };
        let tip = s.tooltip();
        assert!(tip.contains("not responding"), "{tip}");
        assert!(tip.contains("plug it back in"), "{tip}");
        assert!(
            !tip.contains("battery unknown"),
            "an unresponsive dongle should not report on a battery it cannot read: {tip}"
        );
        assert!(tip.len() <= 127, "szTip limit: {}", tip.len());
    }

    #[test]
    fn the_device_snapshot_carries_the_silent_flag() {
        // The worker owns this field. If `apply_device_snapshot` does not copy
        // it, the UI thread keeps its own stale `false` and the warning never
        // appears -- the same class of bug the mic/vendor split documents.
        let mut ui = HeadsetState::default();
        let worker = HeadsetState {
            dongle_silent: true,
            ..HeadsetState::default()
        };
        ui.apply_device_snapshot(&worker);
        assert!(ui.dongle_silent);
    }

    #[test]
    fn a_refused_value_leaves_the_field_unknown() {
        let mut s = HeadsetState::default();
        s.apply(&V3_PRO, &frame(param_id(Param::Battery), &[0xFF]));
        assert_eq!(s.battery, None, "255 is not a battery level");
    }

    #[test]
    fn losing_the_link_clears_proxied_values() {
        let mut s = HeadsetState::default();
        s.apply(&V3_PRO, &frame(param_id(Param::Battery), &[52]));
        s.apply(&V3_PRO, &frame(param_id(Param::Sidetone), &[7]));
        s.apply(&V3_PRO, &frame(param_id(Param::LinkState), &[0x00, 0x00]));
        assert_eq!(s.connected, Some(false));
        assert_eq!(s.battery, None, "a stale battery reading must not persist");
        assert_eq!(s.sidetone, None);
    }

    #[test]
    fn regaining_the_link_does_not_invent_values() {
        let mut s = HeadsetState::default();
        s.apply(&V3_PRO, &frame(param_id(Param::LinkState), &[0x01, 0x00]));
        assert_eq!(s.connected, Some(true));
        assert_eq!(s.battery, None, "connected is not the same as known");
    }

    #[test]
    fn either_mute_source_counts_as_muted() {
        let mut s = HeadsetState::default();
        assert_eq!(s.effectively_muted(), None);

        s.mic_mute_hardware = Some(false);
        s.mic_mute_os = Some(false);
        assert_eq!(s.effectively_muted(), Some(false));

        s.mic_mute_os = Some(true);
        assert_eq!(s.effectively_muted(), Some(true), "OS mute silences audio");

        s.mic_mute_os = Some(false);
        s.mic_mute_hardware = Some(true);
        assert_eq!(
            s.effectively_muted(),
            Some(true),
            "the hardware switch silences audio too"
        );
    }

    #[test]
    fn tooltip_says_off_rather_than_showing_a_stale_battery() {
        let mut s = HeadsetState {
            battery: Some(52),
            ..Default::default()
        };
        s.apply(&V3_PRO, &frame(param_id(Param::LinkState), &[0x00, 0x00]));
        let t = s.tooltip();
        assert!(t.contains("off"), "{t}");
        assert!(!t.contains("52"), "{t}");
    }

    #[test]
    fn tooltip_stays_within_the_windows_limit() {
        let s = HeadsetState {
            connected: Some(true),
            battery: Some(100),
            mic_mute_hardware: Some(true),
            warn_vendor_software: true,
            ..Default::default()
        };
        assert!(s.tooltip().chars().count() < 128, "{}", s.tooltip());
    }

    #[test]
    fn a_device_snapshot_does_not_clobber_ui_owned_fields() {
        // The exact reported bug: turn the Synapse warning off, hit Refresh,
        // and the warning came back because the worker's snapshot still said
        // it was on. Same class of fault silently reverted the OS mute.
        let mut ui = HeadsetState {
            warn_vendor_software: false,
            mic_mute_os: Some(true),
            battery: Some(10),
            ..Default::default()
        };
        let from_worker = HeadsetState {
            warn_vendor_software: true,
            mic_mute_os: None,
            battery: Some(54),
            connected: Some(true),
            ..Default::default()
        };

        ui.apply_device_snapshot(&from_worker);

        assert_eq!(ui.battery, Some(54), "device fields must update");
        assert_eq!(ui.connected, Some(true));
        assert_eq!(ui.noise, from_worker.noise, "noise is the worker's to own");
        assert!(
            !ui.warn_vendor_software,
            "the user turned the warning off; a device refresh must not turn it back on"
        );
        assert_eq!(
            ui.mic_mute_os,
            Some(true),
            "the OS mute is the UI's to own; the worker never reads it"
        );
    }

    #[test]
    fn the_device_name_comes_from_the_descriptor_not_a_hardcoded_model() {
        let s = HeadsetState {
            device_name: Some("BlackShark V3 Pro PS HID".into()),
            ..Default::default()
        };
        assert_eq!(s.device_name(), "BlackShark V3 Pro PS");
        assert_eq!(HeadsetState::default().device_name(), "Headset");
    }

    #[test]
    fn the_noise_state_comes_from_both_payload_bytes() {
        use headset_protocol::v3pro::noise::NoiseMode;
        let mut s = HeadsetState::default();
        s.apply(
            &V3_PRO,
            &frame(param_id(Param::NoiseCancellation), &[0x01, 0x03]),
        );
        let n = s.noise.expect("a two-byte payload is a noise state");
        assert_eq!(n.mode, NoiseMode::Anc);
        assert_eq!(n.anc_level, 3);
    }

    #[test]
    fn a_refused_noise_read_leaves_it_unknown() {
        let mut s = HeadsetState::default();
        s.apply(
            &V3_PRO,
            &frame(param_id(Param::NoiseCancellation), &[0x01, 0x03]),
        );
        s.apply(&V3_PRO, &frame(param_id(Param::NoiseCancellation), &[0xFF]));
        assert_eq!(s.noise, None, "a refusal is not a noise state");
    }

    #[test]
    fn losing_the_link_clears_the_noise_state_too() {
        let mut s = HeadsetState::default();
        s.apply(
            &V3_PRO,
            &frame(param_id(Param::NoiseCancellation), &[0x01, 0x04]),
        );
        s.apply(&V3_PRO, &frame(param_id(Param::LinkState), &[0x00, 0x00]));
        assert_eq!(s.noise, None, "a stale mode must not persist");
    }

    #[test]
    fn a_pending_noise_write_is_shown_until_the_device_answers() {
        use headset_protocol::v3pro::noise::{NoiseControl, NoiseMode};
        let mut s = HeadsetState::default();
        s.apply(
            &V3_PRO,
            &frame(param_id(Param::NoiseCancellation), &[0x01, 0x03]),
        );

        let asked = NoiseControl {
            mode: NoiseMode::Ambient,
            anc_level: 3,
        };
        let shown = s.with_pending_noise(Some(asked));
        assert_eq!(
            shown.noise,
            Some(asked),
            "the panel shows what was asked for"
        );
        assert_eq!(
            s.noise.map(|n| n.mode),
            Some(NoiseMode::Anc),
            "the device's own state is not overwritten by the request"
        );
    }

    #[test]
    fn no_pending_write_leaves_the_device_state_alone() {
        let mut s = HeadsetState::default();
        s.apply(
            &V3_PRO,
            &frame(param_id(Param::NoiseCancellation), &[0x01, 0x03]),
        );
        assert_eq!(s.with_pending_noise(None), s);
    }

    #[test]
    fn a_pending_write_does_not_invent_a_state_while_disconnected() {
        use headset_protocol::v3pro::noise::{NoiseControl, NoiseMode};
        // Nothing was ever read, so there is nothing to preview against. The
        // panel must keep showing "--" rather than a value the device never
        // reported and may refuse.
        let s = HeadsetState::default();
        let asked = NoiseControl {
            mode: NoiseMode::Anc,
            anc_level: 2,
        };
        assert_eq!(s.with_pending_noise(Some(asked)).noise, None);
    }

    #[test]
    fn unidentified_parameters_are_ignored() {
        let mut s = HeadsetState::default();
        s.apply(&V3_PRO, &frame(0x2C, &[0x0F]));
        assert_eq!(s, HeadsetState::default());
    }
}
