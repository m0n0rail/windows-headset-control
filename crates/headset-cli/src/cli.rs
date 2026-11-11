use clap::{Args, Parser, Subcommand, ValueEnum};

/// Accepts `0x1532`, `1532`, and `101b`. Always hexadecimal: USB IDs are
/// universally written in hex, so a decimal reading would silently mislead.
pub fn parse_u16_id(s: &str) -> Result<u16, String> {
    let t = s.trim();
    let body = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{s}` is not a hexadecimal USB id"));
    }
    u16::from_str_radix(body, 16).map_err(|_| format!("`{s}` does not fit in 16 bits"))
}

#[derive(Parser, Debug)]
#[command(
    name = "headsetctl",
    about = "Experimental native Windows HID controller for supported wireless headset settings.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Reveal device paths. Output will contain machine-identifying data.
    #[arg(long, global = true)]
    pub include_sensitive: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Enumerate HID collections. Sends nothing and opens nothing for I/O.
    List(ListArgs),
    /// Read descriptors for one enumerated collection. Never writes.
    Inspect(InspectArgs),
    /// Read-only protocol probe. Performs no HID writes.
    Probe(ProbeArgs),
    /// Read one named parameter from the headset.
    Get(GetArgs),
    /// Write one named parameter to the headset.
    Set(SetArgs),
    /// Show or change noise cancellation. With no options, only reads.
    Noise(NoiseArgs),
    /// Read or write a parameter by its raw identifier.
    Param(ParamArgs),
    /// Print decoded events as the headset pushes them.
    Watch(WatchArgs),
}

/// Accepts `0x21`, `21`, and `0X21`. Always hexadecimal, matching how parameter
/// identifiers are written in `docs/device-research.md`.
pub fn parse_u8_id(s: &str) -> Result<u8, String> {
    let t = s.trim();
    let body = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{s}` is not a hexadecimal parameter id"));
    }
    u8::from_str_radix(body, 16).map_err(|_| format!("`{s}` does not fit in 8 bits"))
}

/// A parameter value. Decimal, because these are magnitudes a user reasons
/// about (sidetone 7, balance 10), not identifiers.
pub fn parse_value(s: &str) -> Result<u8, String> {
    s.trim()
        .parse::<u8>()
        .map_err(|_| format!("`{s}` is not a value between 0 and 255"))
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// A parameter name: link, battery, sidetone, mic-mute, game-chat,
    /// slider-function, noise-cancellation, power-saving, do-not-disturb,
    /// eq-preset, eq-bank, mic-eq. Which ones exist depends on the headset.
    pub name: String,
}

#[derive(Args, Debug)]
pub struct SetArgs {
    /// A writable parameter: sidetone, game-chat and slider-function on the
    /// V3 Pro; sidetone, power-saving, do-not-disturb and mic-eq on the V2 Pro.
    pub name: String,

    /// The value to write.
    #[arg(value_parser = parse_value)]
    pub value: u8,
}

/// The three noise-control modes observed on the wire.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum NoiseModeArg {
    Off,
    Anc,
    Ambient,
}

/// Both fields are optional because the device holds mode and level in one
/// two-byte parameter: whichever is omitted is read from the device and sent
/// back unchanged, which is what the vendor software was observed doing.
#[derive(Args, Debug)]
pub struct NoiseArgs {
    /// off, anc, or ambient. Omitted leaves the current mode alone.
    #[arg(long, value_enum)]
    pub mode: Option<NoiseModeArg>,

    /// ANC strength, 1-4. Ambient has no level. Omitted leaves it alone.
    #[arg(long, value_parser = parse_value)]
    pub level: Option<u8>,
}

#[derive(Args, Debug)]
pub struct ParamArgs {
    #[command(subcommand)]
    pub action: ParamAction,
}

#[derive(Subcommand, Debug)]
pub enum ParamAction {
    /// Read a raw parameter id from the observed read allowlist.
    Get {
        #[arg(value_parser = parse_u8_id)]
        id: u8,

        /// Index operand, for the parameters observed to take one.
        #[arg(long)]
        index: Option<u8>,
    },
    /// Write a raw parameter id from the observed write allowlist.
    Set {
        #[arg(value_parser = parse_u8_id)]
        id: u8,

        #[arg(value_parser = parse_value)]
        value: u8,
    },
}

#[derive(Args, Debug)]
pub struct WatchArgs {
    /// How long to listen, in seconds.
    #[arg(long, default_value_t = 30)]
    pub seconds: u64,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Show only this vendor id, e.g. 0x1532.
    #[arg(long, value_parser = parse_u16_id)]
    pub vendor_id: Option<u16>,

    /// Show only this product id, e.g. 0x101b.
    #[arg(long, value_parser = parse_u16_id)]
    pub product_id: Option<u16>,
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    /// Index from `headsetctl list`.
    #[arg(long)]
    pub path_index: usize,
}

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// Candidate index from `headsetctl list`. Defaults to the ranked best
    /// among the collections of the one device this project supports.
    #[arg(long)]
    pub candidate: Option<usize>,

    /// Restrict automatic candidate selection to this vendor id, e.g. 0x1532.
    /// Opts into a specific device in place of the built-in supported-device
    /// allowlist; has no effect when `--candidate` is also given.
    #[arg(long, value_parser = parse_u16_id)]
    pub vendor_id: Option<u16>,

    /// Restrict automatic candidate selection to this product id, e.g. 0x101b.
    /// Opts into a specific device in place of the built-in supported-device
    /// allowlist; has no effect when `--candidate` is also given.
    #[arg(long, value_parser = parse_u16_id)]
    pub product_id: Option<u16>,

    /// Milliseconds to listen for an unsolicited input report.
    #[arg(long, default_value_t = 2000)]
    pub listen_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_prefix() {
        assert_eq!(parse_u16_id("0x1532").unwrap(), 0x1532);
        assert_eq!(parse_u16_id("0X1532").unwrap(), 0x1532);
    }

    #[test]
    fn parses_bare_hex() {
        assert_eq!(parse_u16_id("1532").unwrap(), 0x1532);
        assert_eq!(parse_u16_id("101b").unwrap(), 0x101B);
    }

    #[test]
    fn rejects_out_of_range() {
        assert!(parse_u16_id("0x10000").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_u16_id("zzzz").is_err());
        assert!(parse_u16_id("").is_err());
        assert!(parse_u16_id("-1").is_err());
        // A bare prefix with no digits after it: the `strip_prefix`-then-check
        // ordering is the one place this could silently regress, e.g. if the
        // emptiness check moved before the prefix strip and matched on `s`
        // instead of `body`.
        assert!(parse_u16_id("0x").is_err());
    }
}
