//! What every supported device's protocol has in common, and the seam between
//! them.
//!
//! The vocabulary here — [`Param`], [`Role`], [`ParamFrame`] — names *what* a
//! setting is. It deliberately says nothing about *how* any device encodes it:
//! which identifier a parameter has, what range it takes, how a request is
//! framed, and which identifiers may be sent at all are each device's own
//! observed facts, and live behind [`Codec`].
//!
//! A device that has no evidence for a parameter does not map it. A missing
//! mapping is how "this device has no such setting" is expressed, so no caller
//! can reach an identifier by assuming two devices share one.

use crate::error::ProtocolError;

/// The settings whose meaning is established on at least one device.
///
/// A variant exists here only where behaviour was observed to change with the
/// value on some device. Observed-but-unidentified parameters are intentionally
/// absent and are reached through their raw identifier instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    LinkState,
    Battery,
    Sidetone,
    GameChatBalance,
    MicMute,
    SliderFunction,
    /// Mode and ANC level together. Two payload bytes on the V3 Pro, so it is
    /// read and written through [`crate::v3pro::noise`] rather than as a
    /// single value.
    NoiseCancellation,
    /// Minutes of inactivity on battery before the headset turns itself off;
    /// `0` is off.
    PowerSavingTimeout,
    DoNotDisturb,
    /// Which playback EQ preset is active.
    EqPreset,
    /// Which group of EQ presets [`Param::EqPreset`] selects from.
    EqBank,
    /// Which microphone EQ preset is active.
    MicEqMode,
}

impl Param {
    pub const ALL: [Param; 12] = [
        Param::LinkState,
        Param::Battery,
        Param::Sidetone,
        Param::GameChatBalance,
        Param::MicMute,
        Param::SliderFunction,
        Param::NoiseCancellation,
        Param::PowerSavingTimeout,
        Param::DoNotDisturb,
        Param::EqPreset,
        Param::EqBank,
        Param::MicEqMode,
    ];

    /// CLI-facing name. Stable; used in both renderers.
    pub fn name(self) -> &'static str {
        match self {
            Param::LinkState => "link",
            Param::Battery => "battery",
            Param::Sidetone => "sidetone",
            Param::GameChatBalance => "game-chat",
            Param::MicMute => "mic-mute",
            Param::SliderFunction => "slider-function",
            Param::NoiseCancellation => "noise-cancellation",
            Param::PowerSavingTimeout => "power-saving",
            Param::DoNotDisturb => "do-not-disturb",
            Param::EqPreset => "eq-preset",
            Param::EqBank => "eq-bank",
            Param::MicEqMode => "mic-eq",
        }
    }

    pub fn from_name(s: &str) -> Option<Param> {
        Param::ALL.into_iter().find(|p| p.name() == s)
    }
}

/// Whether a frame is a request, the answer to one, or something the device
/// said unprompted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Request,
    Response,
    Event,
}

/// What a write's acknowledgement says about the write.
///
/// Which bytes mean what is a per-device observation, so a [`Codec`] produces
/// this; anything it has no evidence for is [`ResultCode::Unknown`] rather than
/// being assumed to mean success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultCode {
    Ok,
    Refused,
    Unknown(u8),
}

impl ResultCode {
    pub fn is_ok(self) -> bool {
        matches!(self, ResultCode::Ok)
    }
}

/// A parsed response or event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamFrame {
    /// Bare parameter id, with the write bit stripped.
    pub param: u8,
    /// Whether the frame answers a write (bit 7 was set on the wire).
    pub is_write: bool,
    pub role: Role,
    pub payload: Vec<u8>,
}

impl ParamFrame {
    /// Value byte for the common single-byte case.
    pub fn value(&self) -> Option<u8> {
        match self.payload.len() {
            1 => Some(self.payload[0]),
            _ => None,
        }
    }

    pub fn hex_payload(&self) -> String {
        self.payload
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One device's protocol: its identifiers, its allowlists, and its framing.
///
/// Everything a caller sends goes through `encode_read` or `encode_write`, and
/// both refuse an identifier that device was not observed accepting. That is
/// the guarantee the allowlists gave when there was one device, kept per device
/// now that a single global list can no longer be right for all of them.
pub trait Codec: Sync {
    /// Short stable identifier, for diagnostics and tests.
    fn name(&self) -> &'static str;

    /// This device's identifier for `param`, or `None` when the device has no
    /// established equivalent.
    fn id(&self, param: Param) -> Option<u8>;

    /// The named parameter an identifier stands for on this device, if any.
    fn param_for(&self, id: u8) -> Option<Param> {
        Param::ALL.into_iter().find(|p| self.id(*p) == Some(id))
    }

    /// Inclusive value range where one was established on this device. `None`
    /// means the range is not known, and the value is surfaced without being
    /// validated against an invented bound.
    fn range(&self, param: Param) -> Option<(u8, u8)>;

    /// Whether `value` is one this device was observed taking for `param`.
    /// Defaults to the inclusive [`Codec::range`]; a device whose values have
    /// gaps narrows it.
    fn accepts(&self, param: Param, value: u8) -> bool {
        self.range(param)
            .is_none_or(|(lo, hi)| (lo..=hi).contains(&value))
    }

    /// Whether a read of `param` was ever observed on this device. A parameter
    /// can be named without being readable: some were only ever written, or
    /// only ever pushed by the device.
    fn is_readable(&self, param: Param) -> bool {
        self.id(param)
            .is_some_and(|id| self.read_allowlist().contains(&id))
    }

    /// Whether a write of `param` was ever observed on this device.
    fn is_writable(&self, param: Param) -> bool {
        self.id(param)
            .is_some_and(|id| self.write_allowlist().contains(&id))
    }

    /// Whether `param` may be written on its own. `false` for a parameter that
    /// was only ever written as part of a longer sequence, which a single
    /// write would not reproduce.
    fn standalone_write(&self, param: Param) -> bool {
        self.is_writable(param)
    }

    /// Identifiers this device was observed being read with.
    fn read_allowlist(&self) -> &'static [u8];

    /// Identifiers seen only as unprompted device events: named so that an
    /// event can be understood, never sent.
    fn event_only(&self) -> &'static [u8] {
        &[]
    }

    /// Identifiers this device was observed being written with, as bare ids.
    fn write_allowlist(&self) -> &'static [u8];

    /// Readable identifiers observed taking an index operand.
    fn indexed_reads(&self) -> &'static [u8];

    /// The payload length a write of `id` was observed carrying.
    fn write_payload_len(&self, id: u8) -> usize;

    /// A write the vendor software was observed sending immediately before
    /// every write of `param`, as `(id, value)`. Reproducing the observed
    /// sequence is the only behaviour this project can defend.
    fn write_preamble(&self, param: Param) -> Option<(u8, u8)>;

    /// Builds a read request, refusing an identifier never observed being read.
    fn encode_read(&self, id: u8, index: Option<u8>) -> Result<Vec<u8>, ProtocolError>;

    /// Builds a write request from the bare id, refusing an identifier never
    /// observed being written or a payload of a length never seen for it.
    fn encode_write(&self, id: u8, payload: &[u8]) -> Result<Vec<u8>, ProtocolError>;

    /// Decodes one input report into the parameter frames it carries.
    ///
    /// Empty for a well-formed report that holds nothing this device's
    /// parameter family covers; those are not decoded, because their structure
    /// is unverified.
    fn parse(&self, raw: &[u8]) -> Result<Vec<ParamFrame>, ProtocolError>;

    /// Interprets a write acknowledgement, or `None` when it carries no single
    /// result byte.
    fn write_result(&self, ack: &ParamFrame) -> Option<ResultCode>;

    /// Whether a read reply for `param` is this device's way of saying "no
    /// value" rather than a value. Defaults to never: a refusal is a per-device
    /// observation, and the same byte can be a real value elsewhere.
    fn read_refused(&self, _param: Param, _reply: &ParamFrame) -> bool {
        false
    }

    /// Whether the dongle answers a read of [`Param::LinkState`] by itself,
    /// with the headset off. Where it does, silence on that read means the
    /// dongle has stopped talking; where it does not, silence is what an
    /// unreachable headset looks like, and the two cannot be told apart.
    fn link_answered_without_headset(&self) -> bool {
        false
    }
}
