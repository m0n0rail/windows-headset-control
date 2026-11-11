//! All `unsafe` in this crate is confined to this module.
//!
//! `read_caps` and `open_for_descriptors` open devices with
//! `dwDesiredAccess = 0`, which grants no read or write rights and therefore
//! cannot contend with the audio stack. `open_for_read` requests read access
//! only (`FILE_GENERIC_READ`) and is used solely by the read-only transport in
//! `transport.rs`; nothing in this crate ever requests write access.

use std::ffi::c_void;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetManufacturerString,
    HidD_GetPreparsedData, HidD_GetProductString, HidD_GetSerialNumberString, HidD_SetOutputReport,
    HidP_Feature, HidP_GetButtonCaps, HidP_GetCaps, HidP_GetValueCaps, HidP_Input, HidP_Output,
    HIDD_ATTRIBUTES, HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_REPORT_TYPE, HIDP_VALUE_CAPS,
    PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_IO_PENDING,
    ERROR_NO_MORE_ITEMS, HANDLE, NTSTATUS, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

use crate::error::DeviceError;
use crate::model::{ReportItem, ReportKind};

const HIDP_STATUS_SUCCESS: NTSTATUS = NTSTATUS(0x0011_0000u32 as i32);

/// Raw descriptor facts for one collection.
pub struct RawCaps {
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u16,
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

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Lists every present HID device interface path.
///
/// `SetupDiEnumDeviceInterfaces` failing is only the normal end-of-enumeration
/// signal when `GetLastError() == ERROR_NO_MORE_ITEMS`. Any other failure code
/// is a real, transient OS-level failure partway through the walk — treating
/// it the same as end-of-enumeration would silently truncate the result and,
/// because every caller treats these paths as a positionally stable list,
/// silently shift the index of every collection enumerated after the failure
/// point. That is the same "an index quietly stops meaning what it meant"
/// property two earlier fix rounds established for the filtering path; here
/// it is surfaced as an `Err` instead.
pub fn enumerate_interface_paths() -> Result<Vec<String>, DeviceError> {
    let mut paths = Vec::new();
    unsafe {
        let hid_guid: GUID = HidD_GetHidGuid();
        let devinfo = SetupDiGetClassDevsW(
            Some(&hid_guid),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(|e| DeviceError::Os(e.to_string()))?;

        let mut index = 0u32;
        loop {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(devinfo, None, &hid_guid, index, &mut iface).is_err() {
                let code = GetLastError();
                if code != ERROR_NO_MORE_ITEMS {
                    let _ = SetupDiDestroyDeviceInfoList(devinfo);
                    return Err(DeviceError::Os(format!(
                        "SetupDiEnumDeviceInterfaces failed with {:#010x} before \
                         ERROR_NO_MORE_ITEMS; enumeration truncated at index {index}",
                        code.0
                    )));
                }
                break;
            }
            index += 1;

            let mut required: u32 = 0;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                devinfo,
                &iface,
                None,
                0,
                Some(&mut required),
                None,
            );
            // 6 = offset_of!(DevicePath) + one u16 terminator; anything smaller is
            // malformed and would underflow the max_chars computation below.
            if !(6..=4096).contains(&required) {
                continue;
            }

            // u32 matches align_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() == 4, which a
            // Vec<u8> allocation does not guarantee.
            let mut buf = vec![0u32; (required as usize).div_ceil(4)];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(devinfo, &iface, Some(detail), required, None, None)
                .is_err()
            {
                continue;
            }

            let path_ptr = (&raw const (*detail).DevicePath) as *const u16;
            let max_chars = (required as usize
                - std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath))
                / 2;
            let mut len = 0usize;
            while len < max_chars && *path_ptr.add(len) != 0 {
                len += 1;
            }
            paths.push(String::from_utf16_lossy(std::slice::from_raw_parts(
                path_ptr, len,
            )));
        }

        let _ = SetupDiDestroyDeviceInfoList(devinfo);
    }
    Ok(paths)
}

/// Opens with `dwDesiredAccess = 0` and reads descriptor facts. Never performs I/O.
pub fn read_caps(path: &str) -> Option<RawCaps> {
    unsafe {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);

        let handle: HANDLE = CreateFileW(
            PCWSTR(wide.as_ptr()),
            0, // no read, no write
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .ok()?;

        let mut attrs = HIDD_ATTRIBUTES {
            Size: size_of::<HIDD_ATTRIBUTES>() as u32,
            ..Default::default()
        };
        if !HidD_GetAttributes(handle, &mut attrs).as_bool() {
            let _ = CloseHandle(handle);
            return None;
        }

        let mut prep = PHIDP_PREPARSED_DATA::default();
        let mut caps = HIDP_CAPS::default();
        let mut report_items = Vec::new();
        if HidD_GetPreparsedData(handle, &mut prep).as_bool() {
            if HidP_GetCaps(prep, &mut caps) == HIDP_STATUS_SUCCESS {
                report_items = collect_report_items(prep, &caps);
            } else {
                caps = HIDP_CAPS::default();
            }
            let _ = HidD_FreePreparsedData(prep);
        }

        let product = read_string(handle, HidD_GetProductString);
        let manufacturer = read_string(handle, HidD_GetManufacturerString);
        let has_serial = read_string(handle, HidD_GetSerialNumberString).is_some();

        let _ = CloseHandle(handle);

        Some(RawCaps {
            vendor_id: attrs.VendorID,
            product_id: attrs.ProductID,
            version: attrs.VersionNumber,
            usage_page: caps.UsagePage,
            usage: caps.Usage,
            input_report_len: caps.InputReportByteLength,
            output_report_len: caps.OutputReportByteLength,
            feature_report_len: caps.FeatureReportByteLength,
            product,
            manufacturer,
            has_serial,
            report_items,
        })
    }
}

type StringFn = unsafe fn(HANDLE, *mut c_void, u32) -> windows::Win32::Foundation::BOOLEAN;

unsafe fn read_string(handle: HANDLE, f: StringFn) -> Option<String> {
    let mut buf = [0u16; 256];
    if f(
        handle,
        buf.as_mut_ptr() as *mut c_void,
        (buf.len() * 2) as u32,
    )
    .as_bool()
    {
        let s = wide_to_string(&buf);
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    } else {
        None
    }
}

/// Reads declared report items from the parsed descriptor. Sends nothing.
unsafe fn collect_report_items(prep: PHIDP_PREPARSED_DATA, caps: &HIDP_CAPS) -> Vec<ReportItem> {
    let mut items = Vec::new();

    let kinds: [(ReportKind, HIDP_REPORT_TYPE, u16, u16); 3] = [
        (
            ReportKind::Input,
            HidP_Input,
            caps.NumberInputValueCaps,
            caps.NumberInputButtonCaps,
        ),
        (
            ReportKind::Output,
            HidP_Output,
            caps.NumberOutputValueCaps,
            caps.NumberOutputButtonCaps,
        ),
        (
            ReportKind::Feature,
            HidP_Feature,
            caps.NumberFeatureValueCaps,
            caps.NumberFeatureButtonCaps,
        ),
    ];

    for (kind, rtype, n_values, n_buttons) in kinds {
        if n_values > 0 && n_values < 1024 {
            let mut len = n_values;
            let mut vc = vec![HIDP_VALUE_CAPS::default(); n_values as usize];
            if HidP_GetValueCaps(rtype, vc.as_mut_ptr(), &mut len, prep) == HIDP_STATUS_SUCCESS {
                for v in vc.iter().take(len as usize) {
                    let (lo, hi) = if v.IsRange.as_bool() {
                        (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
                    } else {
                        (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage)
                    };
                    items.push(ReportItem {
                        kind,
                        report_id: v.ReportID,
                        usage_page: v.UsagePage,
                        usage_min: lo,
                        usage_max: hi,
                        bit_size: v.BitSize,
                        report_count: v.ReportCount,
                        is_button: false,
                    });
                }
            }
        }

        if n_buttons > 0 && n_buttons < 1024 {
            let mut len = n_buttons;
            let mut bc = vec![HIDP_BUTTON_CAPS::default(); n_buttons as usize];
            if HidP_GetButtonCaps(rtype, bc.as_mut_ptr(), &mut len, prep) == HIDP_STATUS_SUCCESS {
                for b in bc.iter().take(len as usize) {
                    let (lo, hi) = if b.IsRange.as_bool() {
                        (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax)
                    } else {
                        (b.Anonymous.NotRange.Usage, b.Anonymous.NotRange.Usage)
                    };
                    items.push(ReportItem {
                        kind,
                        report_id: b.ReportID,
                        usage_page: b.UsagePage,
                        usage_min: lo,
                        usage_max: hi,
                        bit_size: 0,
                        report_count: 0,
                        is_button: true,
                    });
                }
            }
        }
    }

    items
}

/// Opens a collection for reading only. Never requests write access.
pub fn open_for_read(path: &str) -> Result<HANDLE, DeviceError> {
    unsafe {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
        .map_err(map_win_error)
    }
}

/// Opens a collection for reading and writing.
///
/// Granted only to the control session, after the descriptor-shape gate.
pub fn open_for_read_write(path: &str) -> Result<HANDLE, DeviceError> {
    unsafe {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
        .map_err(map_win_error)
    }
}

/// Sends one output report with `HidD_SetOutputReport`.
///
/// This is a control-pipe SET_REPORT, which is the transfer the vendor software
/// was observed using — not a write to an interrupt OUT endpoint. See the
/// Transport correction in `docs/device-research.md`.
///
/// `buf[0]` must be the report ID. The call is synchronous and never leaves I/O
/// pending on the handle, so it needs none of the cancel-and-synchronize
/// machinery `read_with_timeout` requires.
pub fn write_output_report(handle: HANDLE, buf: &[u8]) -> Result<(), DeviceError> {
    // `HidD_SetOutputReport` takes a mutable pointer although it only reads the
    // buffer. Copy so callers can pass a shared slice rather than being forced
    // to hand over a mutable one they may not own.
    let mut owned = buf.to_vec();
    unsafe {
        if HidD_SetOutputReport(
            handle,
            owned.as_mut_ptr() as *mut c_void,
            owned.len() as u32,
        )
        .as_bool()
        {
            Ok(())
        } else {
            Err(map_win_error(windows::core::Error::from_win32()))
        }
    }
}

/// Sends one output report with `WriteFile`: an interrupt OUT transfer.
///
/// The transfer the BlackShark V2 Pro's vendor software was observed using.
/// The handle is overlapped, so the write is waited for with a bound and, if
/// it does not finish, cancelled and drained before `buf` and the `OVERLAPPED`
/// go out of scope -- the same discipline `read_with_timeout` documents.
pub fn write_with_timeout(
    handle: HANDLE,
    buf: &[u8],
    timeout: std::time::Duration,
) -> Result<(), DeviceError> {
    unsafe {
        let event = CreateEventW(None, true, false, PCWSTR::null())
            .map_err(|e| DeviceError::Os(e.to_string()))?;
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        let started = WriteFile(handle, Some(buf), None, Some(&mut ov));
        if let Err(e) = started {
            let code = e.code().0 as u32 & 0xFFFF;
            if code != ERROR_IO_PENDING.0 {
                let _ = CloseHandle(event);
                return Err(map_win_error(e));
            }
            let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
            let wait = WaitForSingleObject(event, ms);
            if wait != WAIT_OBJECT_0 {
                let _ = CancelIo(handle);
                let mut discarded: u32 = 0;
                let _ = GetOverlappedResult(handle, &ov, &mut discarded, true);
                let _ = CloseHandle(event);
                return if wait == WAIT_TIMEOUT {
                    Err(DeviceError::Timeout(timeout))
                } else {
                    Err(DeviceError::Os(format!(
                        "WaitForSingleObject returned {:#010x}",
                        wait.0
                    )))
                };
            }
        }

        let mut transferred: u32 = 0;
        let ok = GetOverlappedResult(handle, &ov, &mut transferred, false);
        let _ = CloseHandle(event);
        match ok {
            Ok(()) if transferred as usize == buf.len() => Ok(()),
            Ok(()) => Err(DeviceError::Os(format!(
                "interrupt write sent {transferred} of {} bytes",
                buf.len()
            ))),
            Err(e) => Err(map_win_error(e)),
        }
    }
}

/// Opens with `dwDesiredAccess = 0`. Cannot perform I/O.
pub fn open_for_descriptors(path: &str) -> Result<HANDLE, DeviceError> {
    unsafe {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(map_win_error)
    }
}

/// Maps a Windows error to the crate's error type. Shared by the open helpers
/// and by the overlapped-read error path, since both surface the same small
/// set of Win32 error codes worth distinguishing.
fn map_win_error(e: windows::core::Error) -> DeviceError {
    match e.code().0 as u32 & 0xFFFF {
        c if c == ERROR_ACCESS_DENIED.0 => DeviceError::AccessDenied,
        c if c == ERROR_BUSY.0 => DeviceError::Busy,
        _ => DeviceError::Os(e.to_string()),
    }
}

pub fn close(handle: HANDLE) {
    unsafe {
        let _ = CloseHandle(handle);
    }
}

/// Overlapped read with a hard deadline. Cancels the I/O on timeout so the
/// handle can be closed cleanly rather than leaked.
///
/// `ReadFile` on an overlapped handle normally returns `Err` with
/// `ERROR_IO_PENDING` even when the read is proceeding normally — that is the
/// expected asynchronous path, not a failure. Any other error from `ReadFile`
/// (bad handle, device removed, access denied, ...) is a real failure and must
/// be reported immediately rather than being misdiagnosed as a slow device
/// after waiting out the full timeout.
///
/// Whenever the wait does not end with the event already signaled
/// (`WAIT_OBJECT_0`) — a timeout, or any other `WaitForSingleObject` result —
/// the read may still be in flight. `CancelIo` only *requests* cancellation;
/// it returns as soon as the request is queued, not once the kernel has
/// actually finished with the I/O request packet. Until that IRP completes,
/// the driver still holds a pointer to `ov` (this stack frame) and to `buf`
/// (the caller's buffer), and completion — including a plain cancellation —
/// delivers the byte count via a special kernel APC that can land at any
/// later point on this thread, including after this function has returned
/// and `ov` no longer exists. So every non-success wait outcome is followed
/// by a *blocking* `GetOverlappedResult` (`bWait = true`) to synchronize with
/// that completion before `ov` and the event are allowed to go away. The
/// event must still be open when that call is made, since `GetOverlappedResult`
/// waits on `ov.hEvent` internally.
///
/// `CancelIo` is thread-scoped: it cancels only outstanding I/O issued by the
/// calling thread, not by the handle in general (that is what `CancelIoEx`
/// is for). Calling it here is safe only because `WindowsTransport` holds a
/// raw `HANDLE` and is therefore auto-`!Send`/`!Sync` — the handle can never
/// be opened on one thread and read from another, so "the calling thread" and
/// "the thread that issued this `ReadFile`" are always the same thread. This
/// is currently enforced only by an accident of the type's contents (a raw
/// `HANDLE` has no `Send`/`Sync` impl), not by an explicit `!Send` bound.
/// Adding `unsafe impl Send for WindowsTransport` — or anything else that
/// lets this handle cross threads — would silently break that invariant and
/// requires switching this call to `CancelIoEx` first. Getting this wrong
/// previously cost a Critical finding (a use of `ov`/`buf` after this
/// function had returned); do not relax it without re-deriving the safety
/// argument above.
pub fn read_with_timeout(
    handle: HANDLE,
    buf: &mut [u8],
    timeout: std::time::Duration,
) -> Result<usize, DeviceError> {
    unsafe {
        let event = CreateEventW(None, true, false, PCWSTR::null())
            .map_err(|e| DeviceError::Os(e.to_string()))?;
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };

        // lpNumberOfBytesRead is documented as unreliable on overlapped
        // handles (MSDN: pass NULL and use GetOverlappedResult instead) for
        // the same reason as above — it's a stack write the kernel is not
        // guaranteed to make before this call returns. The byte count always
        // comes from GetOverlappedResult below, even on the fast path.
        let started = ReadFile(handle, Some(buf), None, Some(&mut ov));

        if let Err(e) = started {
            let code = e.code().0 as u32 & 0xFFFF;
            if code != ERROR_IO_PENDING.0 {
                // A genuine failure, not the normal pending-I/O path. Report it
                // now instead of falling through to wait out the timeout.
                let _ = CloseHandle(event);
                return Err(map_win_error(e));
            }
        } else {
            // Completed synchronously; the operation is already done, so this
            // does not block.
            let mut transferred: u32 = 0;
            let ok = GetOverlappedResult(handle, &ov, &mut transferred, false);
            let _ = CloseHandle(event);
            return match ok {
                Ok(()) => Ok(transferred as usize),
                Err(e) => Err(map_win_error(e)),
            };
        }

        let ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        let wait = WaitForSingleObject(event, ms);
        if wait != WAIT_OBJECT_0 {
            // The read may still be in flight (timeout) or its outcome is
            // simply unknown (WAIT_FAILED / anything else). Request
            // cancellation, then block until the kernel genuinely finishes
            // with `ov` and `buf` before this frame is allowed to go away.
            // This normally returns ERROR_OPERATION_ABORTED; discarding it is
            // intentional, since either way we report the wait outcome below.
            let _ = CancelIo(handle);
            let mut discarded: u32 = 0;
            let _ = GetOverlappedResult(handle, &ov, &mut discarded, true);
            let _ = CloseHandle(event);
            return if wait == WAIT_TIMEOUT {
                Err(DeviceError::Timeout(timeout))
            } else {
                Err(DeviceError::Os(format!(
                    "WaitForSingleObject returned {:#010x}",
                    wait.0
                )))
            };
        }

        let mut transferred: u32 = 0;
        let ok = GetOverlappedResult(handle, &ov, &mut transferred, false);
        let _ = CloseHandle(event);
        match ok {
            Ok(()) => Ok(transferred as usize),
            Err(e) => Err(map_win_error(e)),
        }
    }
}
