use std::time::Duration;

use windows::Win32::Foundation::HANDLE;

use super::ffi;
use crate::backend::HidTransport;
use crate::error::DeviceError;
use crate::model::WriteMethod;

/// How long an interrupt write may stay pending. A report this size completes
/// in a millisecond or two when the dongle is listening; anything near this
/// means it is not, and the caller is told rather than left waiting.
const WRITE_TIMEOUT: Duration = Duration::from_millis(1000);

/// A handle to one HID collection.
///
/// Whether it can write is decided at construction by the access rights the
/// handle was opened with, not by the caller: `write_report` refuses unless the
/// handle came from `open_for_read_write`. A read-only handle that reached a
/// caller expecting to write fails loudly instead of issuing a write Windows
/// would reject with a less obvious error.
pub struct WindowsTransport {
    handle: HANDLE,
    input_report_len: u16,
    output_report_len: u16,
    /// `None` on a read-only handle.
    write_method: Option<WriteMethod>,
}

impl WindowsTransport {
    pub(super) fn new(handle: HANDLE, input_report_len: u16) -> Self {
        Self {
            handle,
            input_report_len,
            output_report_len: 0,
            write_method: None,
        }
    }

    pub(super) fn new_writable(
        handle: HANDLE,
        input_report_len: u16,
        output_report_len: u16,
        write_method: WriteMethod,
    ) -> Self {
        Self {
            handle,
            input_report_len,
            output_report_len,
            write_method: Some(write_method),
        }
    }
}

impl HidTransport for WindowsTransport {
    fn read_report(&self, buf: &mut [u8], timeout: Duration) -> Result<usize, DeviceError> {
        // Windows requires the read buffer to be at least the declared input
        // report length. A short buffer yields ERROR_INVALID_USER_BUFFER, so
        // reject it here with a message that explains the real constraint.
        if buf.len() < self.input_report_len as usize {
            return Err(DeviceError::UnexpectedDescriptor(format!(
                "read buffer is {} bytes; the collection declares {}",
                buf.len(),
                self.input_report_len
            )));
        }
        ffi::read_with_timeout(self.handle, buf, timeout)
    }

    fn input_report_len(&self) -> u16 {
        self.input_report_len
    }

    fn write_report(&self, buf: &[u8]) -> Result<(), DeviceError> {
        let Some(method) = self.write_method else {
            return Err(DeviceError::WriteNotSupported);
        };
        // Windows requires the buffer to be exactly the declared output report
        // length; a short buffer is rejected by the HID stack with an error that
        // does not name the real cause.
        if buf.len() != self.output_report_len as usize {
            return Err(DeviceError::UnexpectedDescriptor(format!(
                "write buffer is {} bytes; the collection declares an output report of {}",
                buf.len(),
                self.output_report_len
            )));
        }
        match method {
            WriteMethod::SetOutputReport => ffi::write_output_report(self.handle, buf),
            WriteMethod::InterruptOut => ffi::write_with_timeout(self.handle, buf, WRITE_TIMEOUT),
        }
    }

    fn output_report_len(&self) -> u16 {
        self.output_report_len
    }
}

impl Drop for WindowsTransport {
    fn drop(&mut self) {
        ffi::close(self.handle);
    }
}

/// A handle opened with no I/O rights. Exists to prove the collection can be
/// opened and released; it cannot read or write.
pub struct DescriptorHandle {
    handle: HANDLE,
    input_report_len: u16,
}

impl DescriptorHandle {
    pub(super) fn new(handle: HANDLE, input_report_len: u16) -> Self {
        Self {
            handle,
            input_report_len,
        }
    }
}

impl HidTransport for DescriptorHandle {
    fn read_report(&self, _buf: &mut [u8], _timeout: Duration) -> Result<usize, DeviceError> {
        Err(DeviceError::AccessDenied)
    }

    fn input_report_len(&self) -> u16 {
        self.input_report_len
    }
}

impl Drop for DescriptorHandle {
    fn drop(&mut self) {
        ffi::close(self.handle);
    }
}
