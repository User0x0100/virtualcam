//! FFI client of the native C++ DirectShow driver's management ABI v3.
//!
//! The C++ DLL still owns registration (COM, FilterMapper, HKLM metadata) and
//! the DirectShow source itself. Phase 1 of the Rust rewrite replaces this
//! module with Rust-native registration that writes the same metadata, so the
//! public manager API does not change when the FFI disappears.

use core::ffi::c_void;
use core::mem;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

use super::{ManagedCameraId, ManagedCameraInfo};
use crate::platform::windows::wide_nul;
use crate::{Error, Result};

const NAME_CAPACITY: usize = 256;
const ABI_VERSION: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct CameraInfoAbi {
    clsid: ManagedCameraId,
    name: [u16; NAME_CAPACITY],
    reserved_width: u32,
    reserved_height: u32,
    reserved_fps_numerator: u32,
    reserved_fps_denominator: u32,
}

type AbiVersionFn = unsafe extern "system" fn() -> u32;
type RegisterFn = unsafe extern "system" fn(*const u16, u32, u32, u32, u32, *mut CameraInfoAbi) -> i32;
type UnregisterFn = unsafe extern "system" fn(*const ManagedCameraId) -> i32;
type RenameFn = unsafe extern "system" fn(*const ManagedCameraId, *const u16) -> i32;
type GetFn = unsafe extern "system" fn(*const ManagedCameraId, *mut CameraInfoAbi) -> i32;
type EnumCallback = unsafe extern "system" fn(*const CameraInfoAbi, *mut c_void) -> i32;
type EnumerateFn = unsafe extern "system" fn(Option<EnumCallback>, *mut c_void) -> i32;

struct DriverApi {
    module: HMODULE,
    register: RegisterFn,
    unregister: UnregisterFn,
    rename: RenameFn,
    get: GetFn,
    enumerate: EnumerateFn,
}

impl DriverApi {
    fn load(path: &Path) -> Result<Self> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(core::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and remains alive for the call.
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            return Err(crate::platform::windows::last_error("LoadLibraryW(virtualcam-managed.dll)"));
        }

        // SAFETY: The module remains owned by DriverApi. Export names are
        // fixed by exports.def and converted only to their matching ABI.
        unsafe {
            let result = (|| {
                let abi_version: AbiVersionFn = load_symbol(module, b"VirtualCamGetAbiVersion\0")?;
                if abi_version() != ABI_VERSION {
                    return Err(Error::ManagedDriverAbi { symbol: "VirtualCamGetAbiVersion" });
                }
                Ok(Self {
                    module,
                    register: load_symbol(module, b"VirtualCamRegister\0")?,
                    unregister: load_symbol(module, b"VirtualCamUnregister\0")?,
                    rename: load_symbol(module, b"VirtualCamRename\0")?,
                    get: load_symbol(module, b"VirtualCamGet\0")?,
                    enumerate: load_symbol(module, b"VirtualCamEnumerate\0")?,
                })
            })();
            if result.is_err() {
                FreeLibrary(module);
            }
            result
        }
    }
}

impl Drop for DriverApi {
    fn drop(&mut self) {
        if !self.module.is_null() {
            // SAFETY: This object owns the successful LoadLibraryW reference.
            unsafe {
                FreeLibrary(self.module);
            }
            self.module = core::ptr::null_mut();
        }
    }
}

unsafe fn load_symbol<T: Copy>(module: HMODULE, name: &'static [u8]) -> Result<T> {
    // SAFETY: caller provides a live module and NUL-terminated ASCII name.
    let proc = unsafe { GetProcAddress(module, name.as_ptr()) }.ok_or(Error::ManagedDriverAbi {
        symbol: core::str::from_utf8(&name[..name.len() - 1]).unwrap_or("unknown"),
    })?;
    debug_assert_eq!(mem::size_of::<T>(), mem::size_of_val(&proc));
    // SAFETY: Export names are bound to exact function-pointer typedefs above.
    Ok(unsafe { mem::transmute_copy(&proc) })
}

fn registered_driver_path(id: ManagedCameraId) -> Result<PathBuf> {
    let key_path = wide_nul(&format!("CLSID\\{id}\\InprocServer32"));
    let mut key: HKEY = core::ptr::null_mut();
    // SAFETY: `key_path` is NUL-terminated and `key` points to writable
    // output storage. HKCR is a predefined non-owning root handle.
    let status = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, key_path.as_ptr(), 0, KEY_READ, &mut key) };
    if status != 0 {
        return Err(Error::Windows {
            operation: "RegOpenKeyExW(managed InprocServer32)",
            code: status,
        });
    }

    let result = (|| {
        let mut value_type = 0_u32;
        let mut bytes = 0_u32;
        // SAFETY: A null value-name requests the key's default value. The
        // first call queries only its type and byte length.
        let status = unsafe { RegQueryValueExW(key, core::ptr::null(), core::ptr::null(), &mut value_type, core::ptr::null_mut(), &mut bytes) };
        if status != 0 {
            return Err(Error::Windows {
                operation: "RegQueryValueExW(managed InprocServer32 size)",
                code: status,
            });
        }
        if value_type != REG_SZ || bytes < 2 || !bytes.is_multiple_of(2) {
            return Err(Error::ManagedDriverAbi { symbol: "registered InprocServer32 path" });
        }

        let mut buffer = vec![0_u16; bytes as usize / 2];
        // SAFETY: `buffer` has exactly the byte capacity reported by the
        // preceding query and remains writable for the synchronous call.
        let status = unsafe { RegQueryValueExW(key, core::ptr::null(), core::ptr::null(), &mut value_type, buffer.as_mut_ptr().cast::<u8>(), &mut bytes) };
        if status != 0 {
            return Err(Error::Windows {
                operation: "RegQueryValueExW(managed InprocServer32)",
                code: status,
            });
        }
        if value_type != REG_SZ {
            return Err(Error::ManagedDriverAbi { symbol: "registered InprocServer32 path" });
        }
        let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
        if end == 0 {
            return Err(Error::ManagedDriverAbi { symbol: "registered InprocServer32 path" });
        }
        Ok(PathBuf::from(OsString::from_wide(&buffer[..end])))
    })();

    // SAFETY: A successful RegOpenKeyExW returned one owned key handle.
    unsafe {
        RegCloseKey(key);
    }
    result
}

pub(super) fn validate_registered_driver(id: ManagedCameraId) -> Result<()> {
    let path = registered_driver_path(id)?;
    // Loading the actual InprocServer32 target is intentional: the manager
    // DLL path is not authoritative for an already-registered COM class.
    let _driver = DriverApi::load(&path)?;
    Ok(())
}

fn check_hresult(operation: &'static str, hr: i32) -> Result<()> {
    if hr >= 0 { Ok(()) } else { Err(Error::Hresult { operation, code: hr }) }
}

fn from_abi(info: CameraInfoAbi) -> Result<ManagedCameraInfo> {
    let end = info.name.iter().position(|unit| *unit == 0).unwrap_or(NAME_CAPACITY);
    let name = String::from_utf16(&info.name[..end]).map_err(|_| Error::ManagedDriverAbi { symbol: "CameraInfoAbi.name" })?;
    Ok(ManagedCameraInfo { id: info.clsid, name })
}

pub(super) fn register(path: &Path, name: &str) -> Result<ManagedCameraInfo> {
    let driver = DriverApi::load(path)?;
    let name = wide_nul(name);
    let mut info = CameraInfoAbi {
        clsid: ManagedCameraId {
            data1: 0,
            data2: 0,
            data3: 0,
            data4: [0; 8],
        },
        name: [0; NAME_CAPACITY],
        reserved_width: 0,
        reserved_height: 0,
        reserved_fps_numerator: 0,
        reserved_fps_denominator: 0,
    };
    // SAFETY: pointers are valid for the synchronous native call and `info`
    // matches the driver's fixed C ABI layout. Zero format arguments are the
    // ABI-v2 marker for identity-only registration.
    let hr = unsafe { (driver.register)(name.as_ptr(), 0, 0, 0, 0, &mut info) };
    check_hresult("VirtualCamRegister", hr)?;
    from_abi(info)
}

pub(super) fn unregister(path: &Path, id: ManagedCameraId) -> Result<()> {
    let driver = DriverApi::load(path)?;
    // SAFETY: `id` has GUID-compatible repr(C) layout and lives for the call.
    check_hresult("VirtualCamUnregister", unsafe { (driver.unregister)(&id) })
}

pub(super) fn rename(path: &Path, id: ManagedCameraId, name: &str) -> Result<()> {
    let driver = DriverApi::load(path)?;
    let name = wide_nul(name);
    // SAFETY: both pointers remain valid for the synchronous native call.
    check_hresult("VirtualCamRename", unsafe { (driver.rename)(&id, name.as_ptr()) })
}

pub(super) fn get(path: &Path, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
    let driver = DriverApi::load(path)?;
    let mut info = CameraInfoAbi {
        clsid: id,
        name: [0; NAME_CAPACITY],
        reserved_width: 0,
        reserved_height: 0,
        reserved_fps_numerator: 0,
        reserved_fps_denominator: 0,
    };
    // SAFETY: `id` and `info` live for the synchronous native call.
    check_hresult("VirtualCamGet", unsafe { (driver.get)(&id, &mut info) })?;
    from_abi(info)
}

pub(super) fn list(path: &Path) -> Result<Vec<ManagedCameraInfo>> {
    unsafe extern "system" fn collect(info: *const CameraInfoAbi, context: *mut c_void) -> i32 {
        if info.is_null() || context.is_null() {
            return 0;
        }
        // SAFETY: Native enumeration invokes this synchronously with the
        // context pointer supplied below and a live CameraInfoAbi.
        let output = unsafe { &mut *context.cast::<Vec<CameraInfoAbi>>() };
        output.push(unsafe { *info });
        1
    }

    let driver = DriverApi::load(path)?;
    let mut raw = Vec::<CameraInfoAbi>::new();
    // SAFETY: callback and context remain valid until enumeration returns.
    check_hresult("VirtualCamEnumerate", unsafe { (driver.enumerate)(Some(collect), (&mut raw as *mut Vec<CameraInfoAbi>).cast()) })?;
    raw.into_iter().map(from_abi).collect()
}
