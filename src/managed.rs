use core::fmt;
use core::str::FromStr;
use std::path::{Path, PathBuf};

use crate::{Error, ObsFrameRate, Result, VirtualCamera};

/// Stable identifier of one managed DirectShow virtual-camera registration.
///
/// The value is the device's COM CLSID. It can be persisted as the canonical
/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` string returned by [`Display`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ManagedCameraId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl ManagedCameraId {
    pub(crate) fn queue_name_wide(self) -> Vec<u16> {
        format!("Local\\VirtualCamRs-{self}").encode_utf16().chain(core::iter::once(0)).collect()
    }
}

impl fmt::Display for ManagedCameraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            self.data1, self.data2, self.data3, self.data4[0], self.data4[1], self.data4[2], self.data4[3], self.data4[4], self.data4[5], self.data4[6], self.data4[7]
        )
    }
}

impl FromStr for ManagedCameraId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let value = value.strip_prefix('{').and_then(|v| v.strip_suffix('}')).unwrap_or(value);
        let mut parts = value.split('-');
        let p0 = parts.next().ok_or(Error::InvalidCameraId)?;
        let p1 = parts.next().ok_or(Error::InvalidCameraId)?;
        let p2 = parts.next().ok_or(Error::InvalidCameraId)?;
        let p3 = parts.next().ok_or(Error::InvalidCameraId)?;
        let p4 = parts.next().ok_or(Error::InvalidCameraId)?;
        if parts.next().is_some() || p0.len() != 8 || p1.len() != 4 || p2.len() != 4 || p3.len() != 4 || p4.len() != 12 {
            return Err(Error::InvalidCameraId);
        }
        if [p0, p1, p2, p3, p4].into_iter().any(|part| !part.as_bytes().iter().all(u8::is_ascii_hexdigit)) {
            return Err(Error::InvalidCameraId);
        }

        fn hex_u8(pair: &str) -> Result<u8> {
            u8::from_str_radix(pair, 16).map_err(|_| Error::InvalidCameraId)
        }

        let mut data4 = [0_u8; 8];
        data4[0] = hex_u8(&p3[0..2])?;
        data4[1] = hex_u8(&p3[2..4])?;
        for (index, slot) in data4[2..].iter_mut().enumerate() {
            let start = index * 2;
            *slot = hex_u8(&p4[start..start + 2])?;
        }

        Ok(Self {
            data1: u32::from_str_radix(p0, 16).map_err(|_| Error::InvalidCameraId)?,
            data2: u16::from_str_radix(p1, 16).map_err(|_| Error::InvalidCameraId)?,
            data3: u16::from_str_radix(p2, 16).map_err(|_| Error::InvalidCameraId)?,
            data4,
        })
    }
}

/// Registration-time format of one managed NV12 virtual camera.
///
/// Each camera intentionally exposes one fixed NV12 mode. Register another
/// camera when an independent name or format is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCameraConfig {
    /// Friendly name shown by DirectShow camera enumerators.
    pub name: String,
    /// Fixed NV12 width. Must be non-zero and even.
    pub width: u32,
    /// Fixed NV12 height. Must be non-zero and even.
    pub height: u32,
    /// Fixed frame rate advertised by the DirectShow output pin.
    pub frame_rate: ObsFrameRate,
}

impl Default for ManagedCameraConfig {
    fn default() -> Self {
        Self {
            name: "VirtualCam NV12".to_owned(),
            width: 1920,
            height: 1080,
            frame_rate: ObsFrameRate::default(),
        }
    }
}

/// Current registration metadata for one managed camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCameraInfo {
    /// Stable COM CLSID used to address this registration.
    pub id: ManagedCameraId,
    /// Friendly name shown to camera consumers.
    pub name: String,
    /// Fixed NV12 width.
    pub width: u32,
    /// Fixed NV12 height.
    pub height: u32,
    /// Fixed advertised frame rate.
    pub frame_rate: ObsFrameRate,
}

/// Management facade for the native DirectShow camera DLL.
///
/// The DLL is shared by every registered camera. Individual cameras receive
/// independent CLSIDs and independent named shared-memory queues. Registration
/// uses the same DirectShow filter-mapper mechanism as OBS and therefore
/// normally requires an elevated process.
#[derive(Clone, Debug)]
pub struct ManagedCameraManager {
    driver_path: PathBuf,
}

impl ManagedCameraManager {
    /// Creates a manager for one built `virtualcam-managed.dll`.
    pub fn new(driver_path: impl Into<PathBuf>) -> Self {
        Self { driver_path: driver_path.into() }
    }

    /// Returns the native DirectShow DLL used by this manager.
    pub fn driver_path(&self) -> &Path {
        &self.driver_path
    }

    /// Registers a new independent NV12 camera and returns its generated CLSID.
    pub fn register(&self, config: ManagedCameraConfig) -> Result<ManagedCameraInfo> {
        validate_config(&config)?;
        platform::register(&self.driver_path, &config)
    }

    /// Removes exactly one camera registration. Other managed cameras and OBS
    /// Virtual Camera are unaffected.
    pub fn unregister(&self, id: ManagedCameraId) -> Result<()> {
        platform::unregister(&self.driver_path, id)
    }

    /// Changes one camera's DirectShow friendly name without changing its CLSID
    /// or shared-memory queue identity.
    pub fn rename(&self, id: ManagedCameraId, name: impl AsRef<str>) -> Result<()> {
        validate_name(name.as_ref())?;
        platform::rename(&self.driver_path, id, name.as_ref())
    }

    /// Returns all camera registrations owned by this driver family.
    pub fn list(&self) -> Result<Vec<ManagedCameraInfo>> {
        platform::list(&self.driver_path)
    }

    /// Reads one registration by CLSID.
    pub fn get(&self, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
        platform::get(&self.driver_path, id)
    }

    /// Opens the independent NV12 producer queue for one registered camera.
    pub fn open(&self, id: ManagedCameraId) -> Result<VirtualCamera> {
        let info = self.get(id)?;
        VirtualCamera::open_managed(info)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.encode_utf16().count() > 255 || name.encode_utf16().any(|unit| unit == 0) {
        return Err(Error::InvalidCameraName);
    }
    Ok(())
}

fn validate_config(config: &ManagedCameraConfig) -> Result<()> {
    validate_name(&config.name)?;
    if config.width == 0 || config.height == 0 || !config.width.is_multiple_of(2) || !config.height.is_multiple_of(2) || config.width > i32::MAX as u32 || config.height > i32::MAX as u32 {
        return Err(Error::InvalidDimensions);
    }
    let pixels = u64::from(config.width) * u64::from(config.height);
    if pixels > (i32::MAX as u64) / 12 {
        return Err(Error::InvalidDimensions);
    }
    if config.frame_rate.numerator == 0 || config.frame_rate.denominator == 0 {
        return Err(Error::InvalidFrameRate);
    }
    let interval = u128::from(config.frame_rate.denominator) * 10_000_000 / u128::from(config.frame_rate.numerator);
    if interval == 0 || interval > u64::MAX as u128 {
        return Err(Error::InvalidFrameRate);
    }
    Ok(())
}

#[cfg(windows)]
mod platform {
    use core::ffi::c_void;
    use core::mem;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

    use super::{ManagedCameraConfig, ManagedCameraId, ManagedCameraInfo};
    use crate::{Error, ObsFrameRate, Result};

    const NAME_CAPACITY: usize = 256;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CameraInfoAbi {
        clsid: ManagedCameraId,
        name: [u16; NAME_CAPACITY],
        width: u32,
        height: u32,
        fps_numerator: u32,
        fps_denominator: u32,
    }

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

    fn wide_string(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn check_hresult(operation: &'static str, hr: i32) -> Result<()> {
        if hr >= 0 { Ok(()) } else { Err(Error::Hresult { operation, code: hr }) }
    }

    fn from_abi(info: CameraInfoAbi) -> Result<ManagedCameraInfo> {
        let end = info.name.iter().position(|unit| *unit == 0).unwrap_or(NAME_CAPACITY);
        let name = String::from_utf16(&info.name[..end]).map_err(|_| Error::ManagedDriverAbi { symbol: "CameraInfoAbi.name" })?;
        Ok(ManagedCameraInfo {
            id: info.clsid,
            name,
            width: info.width,
            height: info.height,
            frame_rate: ObsFrameRate {
                numerator: info.fps_numerator,
                denominator: info.fps_denominator,
            },
        })
    }

    pub(super) fn register(path: &Path, config: &ManagedCameraConfig) -> Result<ManagedCameraInfo> {
        let driver = DriverApi::load(path)?;
        let name = wide_string(&config.name);
        let mut info = CameraInfoAbi {
            clsid: ManagedCameraId {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            },
            name: [0; NAME_CAPACITY],
            width: 0,
            height: 0,
            fps_numerator: 0,
            fps_denominator: 0,
        };
        // SAFETY: pointers are valid for the synchronous native call and `info`
        // matches the driver's fixed C ABI layout.
        let hr = unsafe { (driver.register)(name.as_ptr(), config.width, config.height, config.frame_rate.numerator, config.frame_rate.denominator, &mut info) };
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
        let name = wide_string(name);
        // SAFETY: both pointers remain valid for the synchronous native call.
        check_hresult("VirtualCamRename", unsafe { (driver.rename)(&id, name.as_ptr()) })
    }

    pub(super) fn get(path: &Path, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
        let driver = DriverApi::load(path)?;
        let mut info = CameraInfoAbi {
            clsid: id,
            name: [0; NAME_CAPACITY],
            width: 0,
            height: 0,
            fps_numerator: 0,
            fps_denominator: 0,
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
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use super::{ManagedCameraConfig, ManagedCameraId, ManagedCameraInfo};
    use crate::{Error, Result};

    pub(super) fn register(_: &Path, _: &ManagedCameraConfig) -> Result<ManagedCameraInfo> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn unregister(_: &Path, _: ManagedCameraId) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn rename(_: &Path, _: ManagedCameraId, _: &str) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn get(_: &Path, _: ManagedCameraId) -> Result<ManagedCameraInfo> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn list(_: &Path) -> Result<Vec<ManagedCameraInfo>> {
        Err(Error::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trips_canonical_guid_text() {
        let text = "{12345678-9ABC-DEF0-1122-334455667788}";
        let id: ManagedCameraId = text.parse().expect("valid CLSID");
        assert_eq!(id.to_string(), text);
        assert_eq!(id, "12345678-9abc-def0-1122-334455667788".parse().expect("case-insensitive CLSID"));
    }

    #[test]
    fn id_rejects_invalid_text() {
        for value in ["", "1234", "{12345678-9ABC-DEF0-1122-33445566778Z}", "12345678-9ABC-DEF0-1122", "12345678-9ABC-DEF0-éé-334455667788"] {
            assert_eq!(value.parse::<ManagedCameraId>(), Err(Error::InvalidCameraId));
        }
    }

    #[test]
    fn managed_config_is_fixed_nv12_mode() {
        let config = ManagedCameraConfig::default();
        assert!(validate_config(&config).is_ok());
        assert!(matches!(validate_config(&ManagedCameraConfig { width: 1919, ..config.clone() }), Err(Error::InvalidDimensions)));
        assert!(matches!(validate_config(&ManagedCameraConfig { name: String::new(), ..config }), Err(Error::InvalidCameraName)));
    }
}
