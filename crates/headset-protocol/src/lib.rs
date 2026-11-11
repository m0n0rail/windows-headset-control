//! Pure protocol logic. No operating-system access.
//!
//! [`codec`] holds what every supported device has in common and the
//! [`Codec`] trait that separates them. Each device's own framing, identifiers
//! and allowlists live in a module of their own: [`v3pro`] for the BlackShark
//! V3 Pro and [`v2pro`] for the BlackShark V2 Pro.
#![forbid(unsafe_code)]

pub mod codec;
pub mod error;
pub mod frame;
pub mod v2pro;
pub mod v3pro;

pub use codec::{Codec, Param, ParamFrame, ResultCode, Role};
pub use error::ProtocolError;
pub use frame::{ControlFrame, CONTROL_PAYLOAD_LEN, CONTROL_REPORT_ID, CONTROL_REPORT_LEN};
