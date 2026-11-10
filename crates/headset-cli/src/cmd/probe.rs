use std::time::Duration;

use anyhow::{bail, Result};
use headset_device::{
    has_unambiguous_winner, is_supported_device, rank_candidates, stable_sort_collections,
    Candidate, CollectionInfo, DeviceError, HidBackend, OpenMode, PROFILES,
};
use headset_protocol::ControlFrame;
use serde_json::json;

use crate::cli::ProbeArgs;
use crate::redact::Redactor;
use crate::render::json::SCHEMA_VERSION;

/// The allowlist of operations `probe` may perform.
///
/// Exactly one variant exists, and it performs no write. Adding a variant that
/// writes requires a documented, reviewed decision. See `docs/device-research.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOp {
    /// Open the control collection for reading and listen for an unsolicited
    /// input report. Sends nothing to the device.
    PassiveListen,
}

/// Minimum spacing intended between repeated device requests. Reserved for
/// the write phase; nothing enforces it yet, because `ProbeOp` currently has
/// no request-emitting variant for it to apply to.
pub const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(250);

pub fn run(
    backend: &dyn HidBackend,
    args: &ProbeArgs,
    r: &Redactor,
    as_json: bool,
) -> Result<String> {
    let op = ProbeOp::PassiveListen;

    let mut all: Vec<CollectionInfo> = backend.enumerate()?;
    if all.is_empty() {
        bail!("no HID collections found; is the dongle connected?");
    }
    stable_sort_collections(&mut all);
    let ranked = rank_candidates(&all);

    // Positive device identification before selecting anything automatically:
    // shape-based ranking alone will happily pick an unrelated vendor's HID
    // collection off the same machine (see docs/device-research.md, "no
    // known-safe request" blocker, and the Task 10 fix-round finding). An
    // explicit `--vendor-id`/`--product-id` opts into a specific device;
    // otherwise selection is scoped to the one device this project supports.
    // `rank_candidates`'s per-collection score and disqualification do not
    // depend on what else is in the set, so filtering its already-sorted
    // output preserves both scores and absolute indices; only the
    // unambiguous-winner decision needs recomputing, over the scoped subset.
    let scoped: Vec<Candidate> = ranked
        .iter()
        .filter(|c| {
            let info = &all[c.index];
            match (args.vendor_id, args.product_id) {
                (None, None) => is_supported_device(info),
                _ => {
                    args.vendor_id.is_none_or(|v| info.vendor_id == v)
                        && args.product_id.is_none_or(|p| info.product_id == p)
                }
            }
        })
        .cloned()
        .collect();

    let index = match args.candidate {
        Some(i) => i,
        None => {
            if scoped.is_empty() {
                let wanted = match (args.vendor_id, args.product_id) {
                    (None, None) => {
                        let products = PROFILES
                            .iter()
                            .map(|p| format!("{:#06x}:{:#06x}", p.vendor_id, p.product_id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "vendor:product in [{products}] \
                             (the devices this project has a profile for)"
                        )
                    }
                    (v, p) => format!(
                        "vendor {}, product {}",
                        v.map_or("<any>".to_string(), |x| format!("{x:#06x}")),
                        p.map_or("<any>".to_string(), |x| format!("{x:#06x}"))
                    ),
                };
                bail!(
                    "no supported device found on this machine (looking for {wanted}); \
                     re-run `headsetctl list` to see what is attached"
                );
            }
            if !has_unambiguous_winner(&scoped) {
                bail!(
                    "no unambiguous control candidate; re-run with --candidate <index> \
                     after reviewing `headsetctl list`"
                );
            }
            scoped
                .iter()
                .find(|c| c.disqualified.is_none())
                .expect("a winner exists")
                .index
        }
    };

    let Some(target) = all.get(index) else {
        bail!(
            "candidate index {index} is out of range; {} collections enumerated",
            all.len()
        );
    };

    if let Some(entry) = ranked.iter().find(|c| c.index == index) {
        if let Some(reason) = &entry.disqualified {
            bail!("candidate {index} is disqualified: {reason}");
        }
    }
    if target.is_audio_stack_collection() {
        bail!("refusing to open the Windows audio collection");
    }

    // `--candidate` is a deliberate escape hatch that bypasses the
    // supported-device scope above (see the scoping comment). It stays
    // read-only and cannot write, but the operator should know when they've
    // pointed it at hardware this project has no protocol knowledge of.
    let supported = is_supported_device(target);
    if !supported {
        eprintln!(
            "warning: candidate {index} (vendor {:#06x}, product {:#06x}) is not a device \
             this project has been tested against; its control protocol is unknown. \
             Proceeding read-only because it was explicitly selected.",
            target.vendor_id, target.product_id
        );
    }

    // Bound the listen window so a silent device cannot hang the process.
    let timeout = Duration::from_millis(args.listen_ms.clamp(100, 30_000));

    // `Read`, not `ReadWrite`: probe's read-only contract is now enforced by the
    // access rights Windows granted, not merely by this code not calling write.
    let transport = backend.open(&target.id, OpenMode::Read)?;
    let declared = transport.input_report_len() as usize;
    if declared == 0 || declared > 1024 {
        bail!("collection declares an implausible input report length of {declared}");
    }
    let mut buf = vec![0u8; declared];

    let outcome = match transport.read_report(&mut buf, timeout) {
        Ok(n) => match ControlFrame::parse(&buf[..n]) {
            Ok(frame) => Outcome::Frame {
                bytes: n,
                hex: frame.hex_payload(),
                known_fields: frame.known_fields(),
            },
            Err(e) => Outcome::Malformed {
                bytes: n,
                error: e.to_string(),
            },
        },
        Err(DeviceError::Timeout(_)) => Outcome::Silent,
        Err(e) => return Err(e.into()),
    };
    drop(transport); // release the handle before rendering

    Ok(if as_json {
        render_json(op, index, target, supported, &outcome, r, timeout)
    } else {
        render_human(op, index, target, supported, &outcome, r, timeout)
    })
}

enum Outcome {
    Frame {
        bytes: usize,
        hex: String,
        known_fields: Vec<(&'static str, u8)>,
    },
    Malformed {
        bytes: usize,
        error: String,
    },
    Silent,
}

fn render_json(
    op: ProbeOp,
    index: usize,
    c: &CollectionInfo,
    supported: bool,
    outcome: &Outcome,
    r: &Redactor,
    timeout: Duration,
) -> String {
    let result = match outcome {
        Outcome::Frame {
            bytes,
            hex,
            known_fields,
        } => {
            let interpreted: serde_json::Map<String, serde_json::Value> = known_fields
                .iter()
                .map(|(name, value)| ((*name).to_string(), json!(value)))
                .collect();
            json!({
                "status": "frame_received",
                "bytes": bytes,
                "payload_hex": hex,
                "interpreted_fields": interpreted,
                "note": "no payload semantics are established for this hardware"
            })
        }
        Outcome::Malformed { bytes, error } => json!({
            "status": "unexpected_frame", "bytes": bytes, "error": error
        }),
        Outcome::Silent => json!({
            "status": "silent",
            "note": "no unsolicited input report within the listen window"
        }),
    };

    serde_json::to_string_pretty(&json!({
        "schema_version": SCHEMA_VERSION,
        "include_sensitive": r.include_sensitive(),
        "operation": format!("{op:?}"),
        "wrote_to_device": false,
        "candidate_index": index,
        "supported_device": supported,
        "usage_page": format!("{:#06x}", c.usage_page),
        "input_report_len": c.input_report_len,
        "path": r.path(&c.id),
        "listen_ms": timeout.as_millis(),
        "result": result,
    }))
    .expect("serialization cannot fail")
}

fn render_human(
    op: ProbeOp,
    index: usize,
    c: &CollectionInfo,
    supported: bool,
    outcome: &Outcome,
    r: &Redactor,
    timeout: Duration,
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if let Some(w) = r.warning_banner() {
        let _ = writeln!(s, "{w}\n");
    }
    let _ = writeln!(s, "probe operation : {op:?}");
    let _ = writeln!(s, "wrote to device : no");
    let _ = writeln!(
        s,
        "candidate       : [{index}] usage page {:#06x}",
        c.usage_page
    );
    let _ = writeln!(
        s,
        "supported device: {}",
        if supported { "yes" } else { "no" }
    );
    let _ = writeln!(s, "path            : {}", r.path(&c.id));
    let _ = writeln!(s, "listen window   : {} ms", timeout.as_millis());
    match outcome {
        Outcome::Frame { bytes, hex, .. } => {
            let _ = writeln!(s, "result          : received {bytes} bytes");
            let _ = writeln!(s, "payload         : {hex}");
            let _ = writeln!(
                s,
                "interpretation  : none - no payload semantics established"
            );
        }
        Outcome::Malformed { bytes, error } => {
            let _ = writeln!(s, "result          : {bytes} bytes, failed validation");
            let _ = writeln!(s, "error           : {error}");
        }
        Outcome::Silent => {
            let _ = writeln!(s, "result          : silent (no unsolicited report)");
            let _ = writeln!(
                s,
                "note            : this is expected. The device may only emit reports in\n\
                 \x20                 response to a request, and no request is known to be safe\n\
                 \x20                 on this hardware yet. See docs/device-research.md."
            );
        }
    }
    s
}
