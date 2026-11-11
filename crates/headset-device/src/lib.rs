//! Windows HID device access behind a mockable backend trait.

pub mod backend;
pub mod error;
pub mod fake;
pub mod model;
pub mod profile;
pub mod select;
pub mod session;

#[cfg(windows)]
pub mod windows;

pub use backend::{HidBackend, HidTransport};
pub use error::DeviceError;
pub use fake::FakeHidBackend;
pub use model::{CollectionInfo, DeviceId, OpenMode, ReportItem, ReportKind, WriteMethod};
pub use profile::{profile_for, ControlShape, DeviceProfile, PROFILES};
pub use select::{
    has_unambiguous_winner, is_supported_device, rank_candidates, stable_sort_collections,
    Candidate,
};
pub use session::{
    resolve_control, resolve_control_device, ControlSession, ResolvedControl,
    DEFAULT_EXCHANGE_TIMEOUT, MIN_REQUEST_INTERVAL,
};

#[cfg(windows)]
pub use windows::WindowsHidBackend;
