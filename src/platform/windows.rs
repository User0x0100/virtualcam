use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INVALID_DATA, GetLastError, HANDLE};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile, VirtualQuery};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW};
use windows_sys::Win32::UI::Shell::{FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

use crate::{Error, Result};

pub(crate) fn last_error(operation: &'static str) -> Error {
    // SAFETY: `GetLastError` has no preconditions and reads thread-local state.
    let code = unsafe { GetLastError() };
    trace_warn!(operation, code, "Win32 operation failed");
    Error::Windows { operation, code }
}

/// Reports whether the immediately preceding successful `Create*` call opened
/// an existing named object instead of creating it.
pub(crate) fn opened_existing_object() -> bool {
    // SAFETY: `GetLastError` has no preconditions and reads thread-local state.
    unsafe { GetLastError() == ERROR_ALREADY_EXISTS }
}

/// Encodes `value` as a NUL-terminated UTF-16 string for wide Win32 APIs.
pub(crate) fn wide_nul(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(core::iter::once(0)).collect()
}

pub(crate) struct OwnedHandle(HANDLE);

// SAFETY: Kernel object handles are process-wide and carry no thread
// affinity. Mutex ownership, the one thread-affine kernel concept used by this
// crate, never outlives a single write call.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    /// Takes ownership of a non-null Win32 kernel handle.
    ///
    /// # Safety
    ///
    /// `raw` must be a uniquely owned handle that is valid for `CloseHandle`.
    pub(crate) unsafe fn from_raw(raw: HANDLE) -> Self {
        debug_assert!(!raw.is_null());
        Self(raw)
    }

    /// Takes ownership of the result of a Win32 create/open call, converting a
    /// null return into that call's `GetLastError`.
    ///
    /// Call this immediately after the Win32 call; on success it touches no
    /// thread-local error state, so [`opened_existing_object`] still applies.
    ///
    /// # Safety
    ///
    /// A non-null `raw` must be a uniquely owned handle valid for `CloseHandle`.
    pub(crate) unsafe fn from_result(raw: HANDLE, operation: &'static str) -> Result<Self> {
        if raw.is_null() {
            Err(last_error(operation))
        } else {
            // SAFETY: Forwarded caller contract for the non-null handle.
            Ok(unsafe { Self::from_raw(raw) })
        }
    }

    pub(crate) const fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `OwnedHandle` uniquely owns this valid kernel handle and
            // closes it exactly once while dropping.
            unsafe {
                CloseHandle(self.0);
            }
            self.0 = std::ptr::null_mut();
        }
    }
}

pub(crate) struct MappedView {
    ptr: NonNull<u8>,
    accessible_len: usize,
}

// SAFETY: A mapped view is ordinary process memory with no thread affinity.
// The owner only hands out raw pointers; every mutation through them is
// serialized by `&mut` access to the owning backend.
unsafe impl Send for MappedView {}

impl MappedView {
    pub(crate) fn map(handle: HANDLE, desired_access: u32, operation: &'static str) -> Result<Self> {
        // SAFETY: `handle` is a live file-mapping handle. Offsets and length are
        // zero, requesting a view of the complete mapping as specified by both
        // upstream protocols.
        let mapped = unsafe { MapViewOfFile(handle, desired_access, 0, 0, 0) };
        let Some(ptr) = NonNull::new(mapped.Value.cast::<u8>()) else {
            return Err(last_error(operation));
        };

        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `ptr` came from a successful `MapViewOfFile`; `info` points to
        // writable storage of the exact structure size expected by VirtualQuery.
        let queried = unsafe { VirtualQuery(ptr.as_ptr().cast(), &mut info, core::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if queried == 0 {
            let error = last_error("VirtualQuery(mapped view)");
            // SAFETY: The successful mapped view is still owned locally and is
            // being unwound before returning the VirtualQuery error.
            unsafe {
                UnmapViewOfFile(mapped);
            }
            return Err(error);
        }

        Ok(Self { ptr, accessible_len: info.RegionSize })
    }

    pub(crate) const fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Returns a typed pointer when `T` fits inside the view at an aligned offset.
    pub(crate) fn ptr_at<T>(&self, offset: usize) -> Option<NonNull<T>> {
        let end = offset.checked_add(core::mem::size_of::<T>())?;
        if end > self.accessible_len {
            return None;
        }
        // SAFETY: `offset..end` was checked to remain inside this mapped view.
        let raw = unsafe { self.ptr.as_ptr().add(offset) };
        if raw.align_offset(core::mem::align_of::<T>()) != 0 {
            return None;
        }
        NonNull::new(raw.cast::<T>())
    }

    pub(crate) const fn accessible_len(&self) -> usize {
        self.accessible_len
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        let address = MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr.as_ptr().cast() };
        // SAFETY: `address` is the base returned by `MapViewOfFile` and this
        // owner unmaps it exactly once before its mapping handle is dropped.
        unsafe {
            UnmapViewOfFile(address);
        }
    }
}

/// Fills `N` bytes from the system-preferred cryptographic RNG.
pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};

    let mut bytes = [0_u8; N];
    let length = u32::try_from(N).map_err(|_| Error::FrameSizeOverflow)?;
    // SAFETY: `bytes` is writable for `length` bytes; a null algorithm handle
    // is valid with BCRYPT_USE_SYSTEM_PREFERRED_RNG.
    let status = unsafe { BCryptGenRandom(core::ptr::null_mut(), bytes.as_mut_ptr(), length, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status < 0 {
        return Err(Error::Hresult {
            operation: "BCryptGenRandom",
            code: status,
        });
    }
    Ok(bytes)
}

/// Returns a random non-zero `u64`.
pub(crate) fn random_nonzero_u64() -> Result<u64> {
    loop {
        let value = u64::from_le_bytes(random_bytes()?);
        if value != 0 {
            return Ok(value);
        }
    }
}

/// `S-1-...` text of the user this process runs as.
pub(crate) fn current_user_sid() -> Result<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = core::ptr::null_mut();
    // SAFETY: Pseudo process handle; the token is owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    // SAFETY: OpenProcessToken returned one owned handle.
    let token = unsafe { OwnedHandle::from_raw(token) };
    let mut buffer = [0_u64; 64];
    let mut length = 0_u32;
    // SAFETY: 512 aligned bytes hold TOKEN_USER plus any SID.
    if unsafe { GetTokenInformation(token.raw(), TokenUser, buffer.as_mut_ptr().cast(), core::mem::size_of_val(&buffer) as u32, &mut length) } == 0 {
        return Err(last_error("GetTokenInformation(TokenUser)"));
    }
    // SAFETY: The buffer now holds a TOKEN_USER.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: *mut u16 = core::ptr::null_mut();
    // SAFETY: Valid SID; the string is LocalFree'd below.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    // SAFETY: NUL-terminated string from the API.
    let units = unsafe { core::slice::from_raw_parts(text, (0..).take_while(|&i| *text.add(i) != 0).count()) };
    let value = String::from_utf16_lossy(units);
    // SAFETY: Allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast()) };
    Ok(value)
}

/// Creation time of the current process as a `FILETIME` packed into `u64`.
pub(crate) fn current_process_start_time() -> Result<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let mut times = [FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 }; 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: The pseudo-handle is always valid; all outputs are writable.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) } == 0 {
        return Err(last_error("GetProcessTimes"));
    }
    Ok(u64::from(times[0].dwHighDateTime) << 32 | u64::from(times[0].dwLowDateTime))
}

/// QueryPerformanceCounter clock reporting nanoseconds, matching OBS's
/// `os_gettime_ns` frame timestamps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct QpcClock {
    frequency: u64,
}

impl QpcClock {
    pub(crate) fn new() -> Result<Self> {
        let mut frequency = 0_i64;
        // SAFETY: `frequency` is valid writable storage for LARGE_INTEGER's i64
        // representation used by windows-sys.
        if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0 {
            return Err(last_error("QueryPerformanceFrequency"));
        }
        match u64::try_from(frequency) {
            Ok(frequency) if frequency != 0 => Ok(Self { frequency }),
            _ => Err(Error::Windows {
                operation: "QueryPerformanceFrequency(non-positive frequency)",
                code: ERROR_INVALID_DATA,
            }),
        }
    }

    /// Current time in 100 ns units (the Windows media clock unit).
    pub(crate) fn now_100ns(self) -> Result<u64> {
        let mut counter = 0_i64;
        // SAFETY: `counter` is writable storage for the API's LARGE_INTEGER.
        if unsafe { QueryPerformanceCounter(&raw mut counter) } == 0 {
            return Err(last_error("QueryPerformanceCounter"));
        }
        Ok(mul_div_u64(u64::try_from(counter).unwrap_or(0), 10_000_000, self.frequency))
    }

    pub(crate) fn now_ns(self) -> Result<u64> {
        let mut counter = 0_i64;
        // SAFETY: `counter` is writable storage for the API's LARGE_INTEGER.
        if unsafe { QueryPerformanceCounter(&raw mut counter) } == 0 {
            return Err(last_error("QueryPerformanceCounter"));
        }
        let counter = u64::try_from(counter).map_err(|_| Error::Windows {
            operation: "QueryPerformanceCounter(negative counter)",
            code: ERROR_INVALID_DATA,
        })?;
        Ok(mul_div_u64(counter, 1_000_000_000, self.frequency))
    }
}

fn mul_div_u64(number: u64, multiplier: u64, divisor: u64) -> u64 {
    let scaled = (u128::from(number) * u128::from(multiplier)) / u128::from(divisor);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

pub(crate) fn registry_key_exists(path: &str) -> bool {
    let wide = wide_nul(path);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated for the call and `key` points to valid
    // output storage. HKCR is a predefined non-owning root handle.
    let status = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, wide.as_ptr(), 0, KEY_READ, &mut key) };
    if status == 0 {
        // SAFETY: A successful open returned an owned registry key handle.
        unsafe {
            RegCloseKey(key);
        }
        true
    } else {
        false
    }
}

/// Returns the current user's roaming AppData directory (`CSIDL_APPDATA`).
pub(crate) fn roaming_app_data_dir() -> Result<PathBuf> {
    let mut raw: *mut u16 = core::ptr::null_mut();
    // SAFETY: `raw` receives a CoTaskMemAlloc string that is freed below on
    // every path, as SHGetKnownFolderPath requires even after failure.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT as u32, core::ptr::null_mut(), &mut raw) };
    let path = if hr >= 0 && !raw.is_null() {
        // SAFETY: On success `raw` is a NUL-terminated UTF-16 string.
        let units = unsafe {
            let length = (0..).take_while(|&index| *raw.add(index) != 0).count();
            core::slice::from_raw_parts(raw, length)
        };
        Ok(PathBuf::from(OsString::from_wide(units)))
    } else {
        Err(Error::Hresult {
            operation: "SHGetKnownFolderPath(FOLDERID_RoamingAppData)",
            code: hr,
        })
    };
    // SAFETY: CoTaskMemFree accepts null and the shell-allocated string.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_conversion_matches_upstream_mul_div() {
        assert_eq!(mul_div_u64(12_345, 1_000_000_000, 10_000_000), 1_234_500);
        assert_eq!(mul_div_u64(u64::MAX, 1_000_000_000, 1), u64::MAX);
    }

    #[test]
    fn qpc_clock_is_monotonic_nanoseconds() {
        let clock = QpcClock::new().expect("QPC is available on supported Windows");
        let first = clock.now_ns().expect("read QPC");
        let second = clock.now_ns().expect("read QPC");
        assert!(first > 0 && second >= first);
    }

    #[test]
    fn roaming_app_data_resolves_to_an_existing_directory() {
        assert!(roaming_app_data_dir().expect("resolve roaming AppData").is_dir());
    }
}
