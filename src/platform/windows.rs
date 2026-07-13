use std::ptr::NonNull;

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0},
    System::{
        Memory::{
            MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, UnmapViewOfFile,
            VirtualQuery,
        },
        Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, RegCloseKey, RegOpenKeyExW},
        Threading::{INFINITE, ReleaseMutex, WaitForSingleObject},
    },
};

use crate::{Error, Result};

pub(crate) fn last_error(operation: &'static str) -> Error {
    // SAFETY: `GetLastError` has no preconditions and reads thread-local state.
    let code = unsafe { GetLastError() };
    trace_warn!(operation, code, "Win32 operation failed");
    Error::Windows { operation, code }
}

pub(crate) struct OwnedHandle(HANDLE);

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

impl MappedView {
    pub(crate) fn map(
        handle: HANDLE,
        desired_access: u32,
        operation: &'static str,
    ) -> Result<Self> {
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
        let queried = unsafe {
            VirtualQuery(
                ptr.as_ptr().cast(),
                &mut info,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 {
            // SAFETY: The successful mapped view is still owned locally and is
            // being unwound before returning the VirtualQuery error.
            unsafe {
                UnmapViewOfFile(mapped);
            }
            return Err(last_error("VirtualQuery(mapped view)"));
        }

        Ok(Self {
            ptr,
            accessible_len: info.RegionSize,
        })
    }

    pub(crate) const fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub(crate) const fn accessible_len(&self) -> usize {
        self.accessible_len
    }
}

impl Drop for MappedView {
    fn drop(&mut self) {
        let address = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.ptr.as_ptr().cast(),
        };
        // SAFETY: `address` is the base returned by `MapViewOfFile` and this
        // owner unmaps it exactly once before its mapping handle is dropped.
        unsafe {
            UnmapViewOfFile(address);
        }
    }
}

pub(crate) struct MutexGuard {
    handle: HANDLE,
    acquired: bool,
}

impl MutexGuard {
    pub(crate) fn acquire(handle: HANDLE, operation: &'static str) -> Result<Self> {
        // SAFETY: `handle` is a live mutex handle with synchronization access.
        let result = unsafe { WaitForSingleObject(handle, INFINITE) };
        match result {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self {
                handle,
                acquired: true,
            }),
            WAIT_FAILED => Err(last_error(operation)),
            _ => Err(Error::Protocol {
                backend: crate::CameraType::UnityCapture,
                detail: "infinite mutex wait returned an unexpected status",
            }),
        }
    }

    pub(crate) fn release(mut self, operation: &'static str) -> Result<()> {
        // Mark released only after the system confirms ownership was relinquished.
        // SAFETY: This guard was constructed only after a successful or abandoned
        // mutex wait, both of which grant ownership to the current thread.
        let released = unsafe { ReleaseMutex(self.handle) };
        if released == 0 {
            return Err(last_error(operation));
        }
        self.acquired = false;
        Ok(())
    }
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if self.acquired {
            // SAFETY: A live guard represents mutex ownership on this thread.
            // Drop is best-effort because it cannot return an error.
            unsafe {
                ReleaseMutex(self.handle);
            }
        }
    }
}

pub(crate) fn registry_key_exists(path: &str) -> bool {
    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
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
