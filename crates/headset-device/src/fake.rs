use std::time::Duration;

use serde::Deserialize;

use crate::backend::{HidBackend, HidTransport};
use crate::error::DeviceError;
use crate::model::{CollectionInfo, DeviceId, OpenMode, ReportItem, ReportKind, WriteMethod};

#[derive(Deserialize)]
struct FixtureRoot {
    collections: Vec<FixtureCollection>,
}

#[derive(Deserialize)]
struct FixtureCollection {
    path: String,
    vendor_id: u16,
    product_id: u16,
    version: u16,
    usage_page: u16,
    usage: u16,
    input_report_len: u16,
    output_report_len: u16,
    feature_report_len: u16,
    product: Option<String>,
    manufacturer: Option<String>,
    has_serial: bool,
    #[serde(default)]
    report_items: Vec<FixtureItem>,
}

#[derive(Deserialize)]
struct FixtureItem {
    kind: String,
    report_id: u8,
    usage_page: u16,
    usage_min: u16,
    usage_max: u16,
    bit_size: u16,
    report_count: u16,
    is_button: bool,
}

/// Hardware-free backend driven by a JSON fixture. Used by every test that
/// exercises filtering, ranking, redaction, or rendering.
pub struct FakeHidBackend {
    collections: Vec<CollectionInfo>,
    /// Input reports handed out by `read_report`, in order, per device path.
    canned_reads: Vec<(String, Vec<u8>)>,
    /// Shared with every transport this backend hands out, so a test can assert
    /// on what reached the wire after the transport has been dropped.
    writes: WriteLog,
    /// The write method of the most recent read-write open.
    opened_with: std::sync::Arc<std::sync::Mutex<Option<WriteMethod>>>,
}

impl FakeHidBackend {
    pub fn from_fixture_str(json: &str) -> Result<Self, DeviceError> {
        let root: FixtureRoot = serde_json::from_str(json)
            .map_err(|e| DeviceError::UnexpectedDescriptor(e.to_string()))?;

        let collections = root
            .collections
            .into_iter()
            .map(|c| {
                let id = DeviceId::new(c.path);
                CollectionInfo {
                    interface_number: id.interface_number(),
                    collection_number: id.collection_number(),
                    id,
                    vendor_id: c.vendor_id,
                    product_id: c.product_id,
                    version: c.version,
                    usage_page: c.usage_page,
                    usage: c.usage,
                    input_report_len: c.input_report_len,
                    output_report_len: c.output_report_len,
                    feature_report_len: c.feature_report_len,
                    product: c.product,
                    manufacturer: c.manufacturer,
                    has_serial: c.has_serial,
                    report_items: c
                        .report_items
                        .into_iter()
                        .map(|i| ReportItem {
                            kind: match i.kind.as_str() {
                                "output" => ReportKind::Output,
                                "feature" => ReportKind::Feature,
                                _ => ReportKind::Input,
                            },
                            report_id: i.report_id,
                            usage_page: i.usage_page,
                            usage_min: i.usage_min,
                            usage_max: i.usage_max,
                            bit_size: i.bit_size,
                            report_count: i.report_count,
                            is_button: i.is_button,
                        })
                        .collect(),
                }
            })
            .collect();

        Ok(Self {
            collections,
            canned_reads: Vec::new(),
            writes: WriteLog::default(),
            opened_with: Default::default(),
        })
    }

    /// Queues an input report to be returned by `read_report` for one device.
    ///
    /// Reports are returned in the order queued, which is how a test scripts an
    /// event arriving before the response it is waiting for.
    pub fn push_read(&mut self, id: &DeviceId, report: Vec<u8>) {
        self.canned_reads.push((id.raw().to_string(), report));
    }

    /// Every report written through a transport this backend handed out.
    /// How the most recent read-write open asked for reports to be written.
    pub fn write_method(&self) -> Option<WriteMethod> {
        *self.opened_with.lock().expect("open log poisoned")
    }

    pub fn writes(&self) -> Vec<Vec<u8>> {
        self.writes.lock().expect("writes log poisoned").clone()
    }
}

type WriteLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

struct FakeTransport {
    input_report_len: u16,
    output_report_len: u16,
    writable: bool,
    reports: Vec<Vec<u8>>,
    cursor: std::cell::Cell<usize>,
    writes: WriteLog,
}

impl HidTransport for FakeTransport {
    fn read_report(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, DeviceError> {
        let i = self.cursor.get();
        let Some(report) = self.reports.get(i) else {
            return Err(DeviceError::Timeout(timeout));
        };
        self.cursor.set(i + 1);
        if buf.len() < report.len() {
            return Err(DeviceError::UnexpectedDescriptor(format!(
                "buffer {} smaller than report {}",
                buf.len(),
                report.len()
            )));
        }
        buf[..report.len()].copy_from_slice(report);
        Ok(report.len())
    }

    fn input_report_len(&self) -> u16 {
        self.input_report_len
    }

    fn write_report(&self, buf: &[u8]) -> Result<(), DeviceError> {
        if !self.writable {
            return Err(DeviceError::WriteNotSupported);
        }
        if buf.len() != self.output_report_len as usize {
            return Err(DeviceError::UnexpectedDescriptor(format!(
                "write buffer is {} bytes; the collection declares an output report of {}",
                buf.len(),
                self.output_report_len
            )));
        }
        self.writes
            .lock()
            .expect("writes log poisoned")
            .push(buf.to_vec());
        Ok(())
    }

    fn output_report_len(&self) -> u16 {
        self.output_report_len
    }
}

impl HidBackend for FakeHidBackend {
    fn enumerate(&self) -> Result<Vec<CollectionInfo>, DeviceError> {
        Ok(self.collections.clone())
    }

    fn open(&self, id: &DeviceId, mode: OpenMode) -> Result<Box<dyn HidTransport>, DeviceError> {
        let c = self
            .collections
            .iter()
            .find(|c| c.id == *id)
            .ok_or(DeviceError::DongleNotFound)?;

        if mode.performs_io() && c.is_audio_stack_collection() {
            return Err(DeviceError::RefusedAudioCollection);
        }
        if let OpenMode::ReadWrite(method) = mode {
            *self.opened_with.lock().expect("open log poisoned") = Some(method);
        }
        if matches!(mode, OpenMode::ReadWrite(_)) && c.output_report_len == 0 {
            return Err(DeviceError::UnexpectedControlShape(
                "collection declares no output report".into(),
            ));
        }

        let reports = self
            .canned_reads
            .iter()
            .filter(|(p, _)| p == id.raw())
            .map(|(_, r)| r.clone())
            .collect();

        Ok(Box::new(FakeTransport {
            input_report_len: c.input_report_len,
            output_report_len: c.output_report_len,
            // Mirrors the real backend: writability follows the access the
            // handle was opened with, so a test cannot write through a
            // read-only open any more than production code can.
            writable: matches!(mode, OpenMode::ReadWrite(_)),
            reports,
            cursor: std::cell::Cell::new(0),
            writes: self.writes.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::HidBackend;

    const FIXTURE: &str = include_str!("../tests/fixtures/blackshark-v3-pro-ps.json");

    #[test]
    fn fixture_yields_four_collections() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        assert_eq!(b.enumerate().unwrap().len(), 4);
    }

    #[test]
    fn fixture_control_collection_has_expected_shape() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let all = b.enumerate().unwrap();
        let c = all
            .iter()
            .find(|c| c.usage_page == 0xFF14)
            .expect("vendor page 0xFF14 present");
        assert_eq!(c.output_report_len, 64);
        assert_eq!(c.feature_report_len, 0);
        assert_eq!(c.report_ids(crate::ReportKind::Output), vec![0x02]);
    }

    #[test]
    fn opening_audio_collection_read_write_is_refused() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let all = b.enumerate().unwrap();
        let audio = all
            .iter()
            .find(|c| c.is_audio_stack_collection())
            .expect("audio collection present");
        let err = b
            .open(&audio.id, OpenMode::ReadWrite(WriteMethod::SetOutputReport))
            .unwrap_err();
        assert!(matches!(err, DeviceError::RefusedAudioCollection));
    }

    #[test]
    fn opening_audio_collection_for_descriptors_is_allowed() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let all = b.enumerate().unwrap();
        let audio = all.iter().find(|c| c.is_audio_stack_collection()).unwrap();
        assert!(b.open(&audio.id, OpenMode::Descriptors).is_ok());
    }

    #[test]
    fn unknown_device_id_is_not_found() {
        let b = FakeHidBackend::from_fixture_str(FIXTURE).unwrap();
        let err = b
            .open(&DeviceId::new("nonexistent"), OpenMode::Descriptors)
            .unwrap_err();
        assert!(matches!(err, DeviceError::DongleNotFound));
    }
}
