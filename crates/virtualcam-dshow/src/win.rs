//! Small Win32 RAII helpers used by the source.

use core::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{FILE_MAP_READ, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_VALUE_TYPE, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
use windows_core::PCWSTR;

pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Owned kernel handle.
pub(crate) struct Handle(pub(crate) HANDLE);

// SAFETY: Kernel handles have no thread affinity.
unsafe impl Send for Handle {}
// SAFETY: Only immutable waits/queries are performed through `&Handle`.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: The handle is owned and closed exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// A read-only view of a named mapping.
pub(crate) struct ReadView {
    ptr: *const u8,
    len: usize,
    _mapping: Handle,
}

// SAFETY: A mapped view is plain process memory without thread affinity.
unsafe impl Send for ReadView {}

impl ReadView {
    /// Opens an existing named mapping read-only; `None` if it does not exist.
    pub(crate) fn open(name: &[u16]) -> Option<Self> {
        // SAFETY: NUL-terminated name; the handle is owned on success.
        let mapping = Handle(unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) }.ok()?);
        // SAFETY: Whole-mapping read view of a live section.
        let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            return None;
        }
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: Query of the view just mapped.
        if unsafe { VirtualQuery(Some(view.Value), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            // SAFETY: Unwinds the successful map.
            let _ = unsafe { UnmapViewOfFile(view) };
            return None;
        }
        Some(Self {
            ptr: view.Value.cast(),
            len: info.RegionSize,
            _mapping: mapping,
        })
    }

    pub(crate) const fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }
}

impl Drop for ReadView {
    fn drop(&mut self) {
        // SAFETY: Base address returned by MapViewOfFile, unmapped once.
        let _ = unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr.cast_mut().cast::<c_void>(),
            })
        };
    }
}

/// QPC in 100 ns units.
pub(crate) fn now_100ns() -> u64 {
    let mut frequency = 0_i64;
    let mut counter = 0_i64;
    // SAFETY: Writable outputs; QPC cannot fail on supported Windows.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut frequency);
        let _ = QueryPerformanceCounter(&mut counter);
    }
    if frequency <= 0 || counter < 0 {
        return 0;
    }
    ((counter as u128 * 10_000_000) / frequency as u128) as u64
}

/// Registry hive for camera records.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Hive {
    CurrentUser,
    LocalMachine,
}

impl Hive {
    fn raw(self) -> HKEY {
        match self {
            Self::CurrentUser => HKEY_CURRENT_USER,
            Self::LocalMachine => HKEY_LOCAL_MACHINE,
        }
    }
}

fn get_value(hive: Hive, key: &str, value: &str, flags: windows::Win32::System::Registry::REG_ROUTINE_FLAGS, capacity: usize) -> Option<Vec<u8>> {
    let key = wide(key);
    let value = wide(value);
    let mut buffer = vec![0_u8; capacity];
    let mut length = capacity as u32;
    let mut kind = REG_VALUE_TYPE(0);
    // SAFETY: NUL-terminated strings and a buffer of `length` bytes.
    let status = unsafe { RegGetValueW(hive.raw(), PCWSTR(key.as_ptr()), PCWSTR(value.as_ptr()), flags, Some(&mut kind), Some(buffer.as_mut_ptr().cast()), Some(&mut length)) };
    if status.is_err() {
        return None;
    }
    buffer.truncate(length as usize);
    Some(buffer)
}

/// Reads a bounded `REG_BINARY` value.
pub(crate) fn reg_binary(hive: Hive, key: &str, value: &str, capacity: usize) -> Option<Vec<u8>> {
    get_value(hive, key, value, RRF_RT_REG_BINARY, capacity)
}

/// Reads a bounded `REG_DWORD` value.
pub(crate) fn reg_u32(hive: Hive, key: &str, value: &str) -> Option<u32> {
    get_value(hive, key, value, RRF_RT_REG_DWORD, 4).and_then(|bytes| <[u8; 4]>::try_from(bytes.as_slice()).ok()).map(u32::from_le_bytes)
}

/// Reads a bounded `REG_SZ` value.
pub(crate) fn reg_string(hive: Hive, key: &str, value: &str) -> Option<String> {
    let bytes = get_value(hive, key, value, RRF_RT_REG_SZ, 1024)?;
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    let end = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
    String::from_utf16(&units[..end]).ok()
}
