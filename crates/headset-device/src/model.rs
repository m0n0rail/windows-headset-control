use serde::Serialize;

/// Opaque handle to one HID collection. Wraps a Windows device interface path.
/// The raw path is machine-identifying and must be redacted before display.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(String);

impl DeviceId {
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// The raw Windows path. Callers must redact before displaying.
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// Parses `&mi_NN` out of the interface path, if present.
    pub fn interface_number(&self) -> Option<u8> {
        parse_hex_token(&self.0, "&mi_")
    }

    /// Parses `&col` followed by hex digits out of the interface path, if present.
    pub fn collection_number(&self) -> Option<u8> {
        parse_hex_token(&self.0, "&col")
    }
}

fn parse_hex_token(path: &str, marker: &str) -> Option<u8> {
    let lower = path.to_ascii_lowercase();
    let start = lower.find(marker)? + marker.len();
    let digits: String = lower[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect();
    if digits.is_empty() {
        return None;
    }
    u8::from_str_radix(&digits, 16).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportKind {
    Input,
    Output,
    Feature,
}

/// One declared item from the parsed report descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReportItem {
    pub kind: ReportKind,
    pub report_id: u8,
    pub usage_page: u16,
    pub usage_min: u16,
    pub usage_max: u16,
    /// Bits per field. Zero for button items.
    pub bit_size: u16,
    /// Number of fields. Zero for button items.
    pub report_count: u16,
    pub is_button: bool,
}

/// Everything readable about one HID collection without performing I/O.
#[derive(Clone, Debug)]
pub struct CollectionInfo {
    pub id: DeviceId,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u16,
    pub interface_number: Option<u8>,
    pub collection_number: Option<u8>,
    pub usage_page: u16,
    pub usage: u16,
    pub input_report_len: u16,
    pub output_report_len: u16,
    pub feature_report_len: u16,
    pub product: Option<String>,
    pub manufacturer: Option<String>,
    pub has_serial: bool,
    pub report_items: Vec<ReportItem>,
}

impl CollectionInfo {
    /// Vendor-defined usage pages occupy 0xFF00..=0xFFFF.
    pub fn is_vendor_defined(&self) -> bool {
        self.usage_page >= 0xFF00
    }

    /// Usage page 0x0B usage 0x05 is the telephony headset collection the
    /// Windows audio stack binds to. Never open this for I/O.
    pub fn is_audio_stack_collection(&self) -> bool {
        self.usage_page == 0x000B && self.usage == 0x0005
    }

    /// Declared report IDs for one report kind, ascending and deduplicated.
    pub fn report_ids(&self, kind: ReportKind) -> Vec<u8> {
        let mut ids: Vec<u8> = self
            .report_items
            .iter()
            .filter(|i| i.kind == kind)
            .map(|i| i.report_id)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// How a writable handle puts an output report on the wire.
///
/// Per device, because it is whatever the vendor software was observed doing:
/// two devices whose reports look alike can still be written differently, and
/// a write the device was never seen receiving is not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMethod {
    /// `HidD_SetOutputReport`: a control-pipe `SET_REPORT`. The BlackShark V3
    /// Pro (`docs/device-research.md`, Transport correction).
    SetOutputReport,
    /// `WriteFile`: an interrupt OUT transfer. The BlackShark V2 Pro
    /// (`docs/device-research-blackshark-v2-pro.md`, Transport).
    InterruptOut,
}

/// How a collection should be opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// `CreateFileW(dwDesiredAccess = 0)`. Cannot perform I/O and therefore
    /// cannot contend with the audio stack.
    Descriptors,
    /// `FILE_GENERIC_READ` only. Can receive input reports; cannot write.
    /// This is what `probe` uses, so its read-only contract is enforced by the
    /// access rights Windows granted, not merely by the absence of a call.
    Read,
    /// `FILE_GENERIC_READ | FILE_GENERIC_WRITE`. Granted only to the control
    /// session, and only after the descriptor-shape gate has passed.
    ///
    /// Phase 1 named this variant for the access it would eventually grant
    /// while requesting read only. The write phase makes the name true; callers
    /// that want the old behaviour want `Read`.
    ReadWrite(WriteMethod),
}

impl OpenMode {
    /// Whether this mode performs I/O at all. Used to decide when opening the
    /// audio-stack collection must be refused.
    pub fn performs_io(self) -> bool {
        matches!(self, OpenMode::Read | OpenMode::ReadWrite(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CollectionInfo {
        CollectionInfo {
            id: DeviceId::new("\\\\?\\hid#vid_1532&pid_101b&mi_05&col04#7&abc&0&0000"),
            vendor_id: 0x1532,
            product_id: 0x101B,
            version: 0x0100,
            interface_number: Some(5),
            collection_number: Some(4),
            usage_page: 0xFF14,
            usage: 0x0001,
            input_report_len: 64,
            output_report_len: 64,
            feature_report_len: 0,
            product: Some("BlackShark V3 Pro PS HID".into()),
            manufacturer: Some("Razer Inc".into()),
            has_serial: true,
            report_items: vec![],
        }
    }

    #[test]
    fn vendor_defined_usage_page_is_detected() {
        assert!(sample().is_vendor_defined());
    }

    #[test]
    fn standard_usage_page_is_not_vendor_defined() {
        let mut c = sample();
        c.usage_page = 0x000B;
        assert!(!c.is_vendor_defined());
    }

    #[test]
    fn audio_stack_collection_is_flagged() {
        let mut c = sample();
        c.usage_page = 0x000B;
        c.usage = 0x0005;
        assert!(c.is_audio_stack_collection());
        assert!(!sample().is_audio_stack_collection());
    }

    #[test]
    fn interface_and_collection_parse_from_path() {
        let id = DeviceId::new("\\\\?\\hid#vid_1532&pid_101b&mi_05&col04#7&abc&0&0000");
        assert_eq!(id.interface_number(), Some(5));
        assert_eq!(id.collection_number(), Some(4));
    }

    #[test]
    fn path_without_interface_yields_none() {
        let id = DeviceId::new("\\\\?\\hid#vid_046d&pid_c52b#6&xyz&0&0000");
        assert_eq!(id.interface_number(), None);
        assert_eq!(id.collection_number(), None);
    }

    #[test]
    fn report_ids_deduplicates_and_sorts() {
        let mut c = sample();
        c.report_items = vec![
            ReportItem {
                kind: ReportKind::Input,
                report_id: 5,
                usage_page: 0xFF00,
                usage_min: 0,
                usage_max: 0,
                bit_size: 8,
                report_count: 1,
                is_button: false,
            },
            ReportItem {
                kind: ReportKind::Input,
                report_id: 2,
                usage_page: 0xFF00,
                usage_min: 0,
                usage_max: 0,
                bit_size: 8,
                report_count: 1,
                is_button: false,
            },
            ReportItem {
                kind: ReportKind::Input,
                report_id: 5,
                usage_page: 0xFF00,
                usage_min: 0,
                usage_max: 0,
                bit_size: 8,
                report_count: 1,
                is_button: false,
            },
            ReportItem {
                kind: ReportKind::Output,
                report_id: 3,
                usage_page: 0xFF00,
                usage_min: 0,
                usage_max: 0,
                bit_size: 8,
                report_count: 1,
                is_button: false,
            },
        ];

        // Input reports: 5, 2, 5 -> deduplicated and sorted -> [2, 5]
        assert_eq!(c.report_ids(ReportKind::Input), vec![2, 5]);
        // Output reports: only 3
        assert_eq!(c.report_ids(ReportKind::Output), vec![3]);
        // Feature reports: none
        assert_eq!(c.report_ids(ReportKind::Feature), Vec::<u8>::new());
    }
}
