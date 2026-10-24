//! Minimal raw-Win32 HID access: enumerate device interfaces via CfgMgr32,
//! open with CreateFile, exchange reports via WriteFile/overlapped ReadFile
//! and HidD_Set/GetFeature. No third-party HID library.
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetPreparsedData,
    HidD_GetProductString, HidD_SetFeature, HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const GUID_DEVINTERFACE_HID: GUID = GUID {
    data1: 0x4d1e55b2,
    data2: 0xf16f,
    data3: 0x11cf,
    data4: [0x88, 0xcb, 0x00, 0x11, 0x11, 0x00, 0x00, 0x30],
};
const ERROR_IO_PENDING: u32 = 997;
const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;

/// Vendors we know how to talk to; other interfaces are skipped before the
/// preparsed-data / product-string queries.
pub const PULSAR_VID: u16 = 0x3710;
pub const VAXEE_VID: u16 = 0x3057;
const VENDOR_IDS: [u16; 2] = [PULSAR_VID, VAXEE_VID];

// `pid` tells receivers from cabled mice; `usage` picks the Pulsar XS-1
// control collection.
#[allow(dead_code)]
pub struct HidDeviceInfo {
    pub path: Vec<u16>, // null-terminated wide string
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub product: String,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
}

/// Owned Win32 HANDLE closed on drop. Holds a raw pointer, so it is `!Send`
/// and `!Sync` by construction — a device can't be shared across threads.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle we own (constructors check for
        // INVALID_HANDLE_VALUE / null before wrapping) and is closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Fetch the REG_MULTI_SZ list of present HID interface paths, or `None`.
fn interface_list() -> Option<Vec<u16>> {
    // Size + list is racy against device arrival (CR_BUFFER_SMALL); retry a few times.
    for _ in 0..4 {
        let mut len: u32 = 0;
        // SAFETY: `len` is a valid out-pointer; the GUID is a `'static` const.
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &GUID_DEVINTERFACE_HID,
                null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS || len == 0 {
            return None;
        }
        let mut buf: Vec<u16> = vec![0; len as usize];
        // SAFETY: `buf` holds exactly `len` u16s, which is the length we pass.
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &GUID_DEVINTERFACE_HID,
                null(),
                buf.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        match cr {
            CR_SUCCESS => return Some(buf),
            CR_BUFFER_SMALL => continue,
            _ => return None,
        }
    }
    None
}

/// Query attributes, caps and product string of one interface. `None` if the
/// vendor is unknown or the collection has no usable caps.
fn probe(wpath: Vec<u16>) -> Option<HidDeviceInfo> {
    // Metadata-only open (desired access 0) works even for keyboard/mouse
    // collections that deny read/write opens.
    // SAFETY: `wpath` is a null-terminated wide string that outlives the call.
    let h = unsafe {
        CreateFileW(
            wpath.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = OwnedHandle(h);

    // SAFETY: HIDD_ATTRIBUTES is plain data for which all-zero is valid; the
    // out-pointer is valid and `Size` is set as the API requires.
    let attrs = unsafe {
        let mut attrs: HIDD_ATTRIBUTES = zeroed();
        attrs.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
        if HidD_GetAttributes(h.0, &mut attrs) == 0 {
            return None;
        }
        attrs
    };
    if !VENDOR_IDS.contains(&attrs.VendorID) {
        return None;
    }

    // SAFETY: HIDP_CAPS is plain data for which all-zero is valid. Preparsed
    // data is freed exactly once, only if it was successfully allocated.
    let caps = unsafe {
        let mut caps: HIDP_CAPS = zeroed();
        let mut preparsed: isize = 0;
        if HidD_GetPreparsedData(h.0, &mut preparsed) == 0 {
            return None;
        }
        let ok = HidP_GetCaps(preparsed, &mut caps) == HIDP_STATUS_SUCCESS;
        HidD_FreePreparsedData(preparsed);
        if !ok {
            return None;
        }
        caps
    };

    let mut name_buf = [0u16; 128];
    // SAFETY: the buffer length is passed in bytes (u16 count * 2), matching
    // what the API expects; the pointer is valid for that many bytes.
    let got_name = unsafe {
        HidD_GetProductString(
            h.0,
            name_buf.as_mut_ptr() as *mut c_void,
            (name_buf.len() * 2) as u32,
        ) != 0
    };
    let product = if got_name {
        let n = name_buf
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(name_buf.len());
        String::from_utf16_lossy(&name_buf[..n])
    } else {
        String::new()
    };

    Some(HidDeviceInfo {
        path: wpath,
        vid: attrs.VendorID,
        pid: attrs.ProductID,
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        product,
        input_len: caps.InputReportByteLength,
        output_len: caps.OutputReportByteLength,
        feature_len: caps.FeatureReportByteLength,
    })
}

/// Enumerate all present HID device interfaces and their top-level collection info.
pub fn enumerate() -> Vec<HidDeviceInfo> {
    let Some(buf) = interface_list() else {
        return Vec::new();
    };
    // buf is a REG_MULTI_SZ: null-terminated strings, double-null at end.
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|path| {
            let mut wpath = path.to_vec();
            wpath.push(0);
            probe(wpath)
        })
        .collect()
}

pub struct HidDevice {
    handle: OwnedHandle,
    event: OwnedHandle,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
}

impl HidDevice {
    pub fn open(info: &HidDeviceInfo) -> Option<HidDevice> {
        // SAFETY: `info.path` is a null-terminated wide string that outlives the call.
        let h = unsafe {
            CreateFileW(
                info.path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let handle = OwnedHandle(h);
        // SAFETY: all-null arguments are valid; manual-reset, initially unsignalled.
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return None; // `handle` is closed by its Drop
        }
        Some(HidDevice {
            handle,
            event: OwnedHandle(event),
            input_len: info.input_len,
            output_len: info.output_len,
            feature_len: info.feature_len,
        })
    }

    /// Wait for the overlapped operation described by `ov` to finish.
    /// Returns the byte count on success, `None` on timeout or failure.
    ///
    /// # Safety
    /// `ov` must be the OVERLAPPED passed to a ReadFile/WriteFile on
    /// `self.handle` that either succeeded or returned ERROR_IO_PENDING.
    unsafe fn finish_overlapped(&self, ov: &mut OVERLAPPED, timeout_ms: u32) -> Option<u32> {
        let mut n = 0u32;
        // SAFETY: `ov` and `n` are live for the whole function; the caller
        // guarantees an I/O is queued on `ov`.
        unsafe {
            if WaitForSingleObject(self.event.0, timeout_ms) != WAIT_OBJECT_0 {
                // Cancel, then *wait* for the cancellation to complete so the
                // kernel never touches `ov` or the data buffer after we return.
                CancelIoEx(self.handle.0, ov);
                GetOverlappedResult(self.handle.0, ov, &mut n, 1);
                return None;
            }
            if GetOverlappedResult(self.handle.0, ov, &mut n, 0) == 0 {
                return None;
            }
        }
        Some(n)
    }

    /// Send an output report. `data[0]` must be the report ID; the buffer is
    /// padded to the collection's OutputReportByteLength.
    pub fn write(&self, data: &[u8]) -> bool {
        let mut buf = vec![0u8; self.output_len.max(data.len() as u16) as usize];
        buf[..data.len()].copy_from_slice(data);
        // SAFETY: OVERLAPPED is plain data for which all-zero is valid. `buf`
        // and `ov` outlive the I/O because `finish_overlapped` does not return
        // until the operation has completed or been cancelled-and-awaited.
        unsafe {
            let mut ov: OVERLAPPED = zeroed();
            ov.hEvent = self.event.0;
            ResetEvent(self.event.0);
            let ok = WriteFile(
                self.handle.0,
                buf.as_ptr(),
                buf.len() as u32,
                null_mut(),
                &mut ov,
            );
            if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                return false;
            }
            self.finish_overlapped(&mut ov, 2000).is_some()
        }
    }

    /// Read one input report with a timeout. Returns the raw report (report ID first).
    pub fn read_timeout(&self, timeout_ms: u32) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; self.input_len as usize];
        // SAFETY: as in `write`; `buf` has exactly `buf.len()` writable bytes
        // and stays alive until the I/O is complete or cancelled-and-awaited.
        let n = unsafe {
            let mut ov: OVERLAPPED = zeroed();
            ov.hEvent = self.event.0;
            ResetEvent(self.event.0);
            let ok = ReadFile(
                self.handle.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                null_mut(),
                &mut ov,
            );
            if ok == 0 && GetLastError() != ERROR_IO_PENDING {
                return None;
            }
            self.finish_overlapped(&mut ov, timeout_ms)?
        };
        buf.truncate(n as usize);
        Some(buf)
    }

    /// Send a feature report. `data[0]` = report ID; padded to FeatureReportByteLength.
    pub fn set_feature(&self, data: &[u8]) -> bool {
        let mut buf = vec![0u8; self.feature_len.max(data.len() as u16) as usize];
        buf[..data.len()].copy_from_slice(data);
        // SAFETY: `buf` is a live, initialised Vec of exactly `buf.len()` bytes.
        unsafe {
            HidD_SetFeature(
                self.handle.0,
                buf.as_ptr() as *const c_void,
                buf.len() as u32,
            ) != 0
        }
    }

    /// Get a feature report for `report_id`. Returns the full buffer (report ID first).
    pub fn get_feature(&self, report_id: u8) -> Option<Vec<u8>> {
        if self.feature_len == 0 {
            return None; // collection has no feature reports
        }
        let mut buf = vec![0u8; self.feature_len as usize];
        buf[0] = report_id;
        // SAFETY: `buf` is a live Vec of exactly `buf.len()` writable bytes.
        let ok = unsafe {
            HidD_GetFeature(
                self.handle.0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            ) != 0
        };
        ok.then_some(buf)
    }
}
