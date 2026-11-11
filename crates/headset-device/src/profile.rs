//! The devices this project supports, and what is known about each.
//!
//! A profile is the whole of what lets this project talk to one product: how to
//! recognise it, what its control collection must look like before anything is
//! written to it, and the codec whose allowlists bound what may be sent. A
//! product is supported exactly when it has a profile here, and every entry
//! needs its own capture evidence in `docs/` — a profile is never added on the
//! strength of another product's protocol.

use headset_protocol::v2pro::V2_PRO;
use headset_protocol::v3pro::V3_PRO;
use headset_protocol::Codec;

use crate::model::{CollectionInfo, WriteMethod};

/// The descriptor shape a profile's protocol was derived from.
///
/// A supported device whose control collection does not match this is refused
/// rather than written to on the assumption that the profile's framing applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlShape {
    pub usage_page: u16,
    /// Including the report-ID byte.
    pub input_report_len: u16,
    /// Including the report-ID byte.
    pub output_report_len: u16,
}

/// One supported product.
pub struct DeviceProfile {
    /// Human-readable product name, for messages. The tray shows the device's
    /// own product string, not this.
    pub name: &'static str,
    pub vendor_id: u16,
    pub product_id: u16,
    pub control: ControlShape,
    /// How output reports reach the device, as the vendor software was observed
    /// sending them.
    pub write_method: WriteMethod,
    pub codec: &'static dyn Codec,
}

impl std::fmt::Debug for DeviceProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceProfile")
            .field("name", &self.name)
            .field("vendor_id", &format_args!("{:#06x}", self.vendor_id))
            .field("product_id", &format_args!("{:#06x}", self.product_id))
            .field("control", &self.control)
            .field("write_method", &self.write_method)
            .field("codec", &self.codec.name())
            .finish()
    }
}

/// Every supported product.
///
/// Do not add `0x0577` or any other id on the strength of public discussion:
/// `docs/device-research.md` explicitly refutes that PID for the V3 Pro, and
/// each entry here stands on its own captures.
pub static PROFILES: &[DeviceProfile] = &[
    DeviceProfile {
        name: "BlackShark V3 Pro",
        vendor_id: 0x1532,
        product_id: 0x101B,
        // Measured with `HidP_GetCaps` on our own hardware; see the Verified
        // Hardware Baseline in `docs/device-research.md`.
        control: ControlShape {
            usage_page: 0xFF14,
            input_report_len: 64,
            output_report_len: 64,
        },
        write_method: WriteMethod::SetOutputReport,
        codec: &V3_PRO,
    },
    DeviceProfile {
        name: "BlackShark V2 Pro",
        vendor_id: 0x1532,
        product_id: 0x0555,
        // Measured with `headsetctl inspect` on the contributor's hardware; see
        // the Verified hardware baseline in
        // `docs/device-research-blackshark-v2-pro.md`.
        control: ControlShape {
            usage_page: 0xFF00,
            input_report_len: 64,
            output_report_len: 64,
        },
        write_method: WriteMethod::InterruptOut,
        codec: &V2_PRO,
    },
];

/// The profile for the product `c` belongs to, if it is supported.
pub fn profile_for(c: &CollectionInfo) -> Option<&'static DeviceProfile> {
    PROFILES
        .iter()
        .find(|p| p.vendor_id == c.vendor_id && p.product_id == c.product_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use headset_protocol::Param;

    #[test]
    fn no_two_profiles_claim_the_same_product() {
        for (i, a) in PROFILES.iter().enumerate() {
            for b in &PROFILES[i + 1..] {
                assert!(
                    (a.vendor_id, a.product_id) != (b.vendor_id, b.product_id),
                    "{} and {} share an id",
                    a.name,
                    b.name
                );
            }
        }
    }

    #[test]
    fn every_named_parameter_a_profile_maps_was_observed_on_it() {
        // A mapping no allowlist covers is a parameter the codec claims with
        // no evidence of ever having seen -- a broken profile, not a missing
        // feature. Read, written, or pushed by the device all count.
        for p in PROFILES {
            for param in Param::ALL {
                if let Some(id) = p.codec.id(param) {
                    let c = p.codec;
                    assert!(
                        c.read_allowlist().contains(&id)
                            || c.write_allowlist().contains(&id)
                            || c.event_only().contains(&id),
                        "{}: {} maps to {id:#04x}, which was never observed",
                        p.name,
                        param.name()
                    );
                }
            }
        }
    }

    #[test]
    fn the_v2_pro_writes_over_interrupt_out_and_the_v3_pro_does_not() {
        let by_pid = |pid| PROFILES.iter().find(|p| p.product_id == pid).unwrap();
        assert_eq!(by_pid(0x101B).write_method, WriteMethod::SetOutputReport);
        assert_eq!(by_pid(0x0555).write_method, WriteMethod::InterruptOut);
    }

    #[test]
    fn every_profile_expects_a_vendor_defined_control_collection() {
        for p in PROFILES {
            assert!(
                p.control.usage_page >= 0xFF00,
                "{} expects usage page {:#06x}",
                p.name,
                p.control.usage_page
            );
        }
    }

    #[test]
    fn the_supported_products_are_pinned() {
        // Pinned so that adding a profile is a deliberate, reviewed change that
        // updates the README's compatibility table at the same time.
        let ids: Vec<(u16, u16, &str)> = PROFILES
            .iter()
            .map(|p| (p.vendor_id, p.product_id, p.codec.name()))
            .collect();
        assert_eq!(
            ids,
            vec![
                (0x1532, 0x101B, "blackshark-v3-pro"),
                (0x1532, 0x0555, "blackshark-v2-pro"),
            ]
        );
    }
}
