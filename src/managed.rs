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

/// Persistent registration metadata for one managed NV12 virtual camera.
///
/// Registration creates only the stable device identity. Width, height, and
/// frame rate are selected later for each producer session with
/// [`ManagedStreamConfig`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCameraConfig {
    /// Friendly name shown by DirectShow camera enumerators.
    pub name: String,
}

impl Default for ManagedCameraConfig {
    fn default() -> Self {
        Self { name: "VirtualCam NV12".to_owned() }
    }
}

/// Runtime format selected before a DirectShow consumer connects.
///
/// One producer session exposes exactly one NV12 media type. A later session
/// may reopen the same registered CLSID with a different format. Changing the
/// format of an already connected DirectShow graph is intentionally unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedStreamConfig {
    /// NV12 width. Must be non-zero and even.
    pub width: u32,
    /// NV12 height. Must be non-zero and even.
    pub height: u32,
    /// Frame rate advertised for this producer session.
    pub frame_rate: ObsFrameRate,
}

impl Default for ManagedStreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frame_rate: ObsFrameRate::default(),
        }
    }
}

/// Current persistent registration metadata for one managed camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCameraInfo {
    /// Stable COM CLSID used to address this registration.
    pub id: ManagedCameraId,
    /// Friendly name shown to camera consumers.
    pub name: String,
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
    ///
    /// No stream format is persisted. The format is selected by [`Self::open`]
    /// before a DirectShow consumer connects.
    pub fn register(&self, config: ManagedCameraConfig) -> Result<ManagedCameraInfo> {
        validate_name(&config.name)?;
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

    /// Opens one producer session with the runtime NV12 format to advertise.
    ///
    /// The queue header is the runtime format declaration consumed by the
    /// DirectShow DLL. Open the producer before the receiving application opens
    /// the camera. Reopen after the previous session ends to use another format
    /// with the same CLSID.
    pub fn open(&self, id: ManagedCameraId, stream: ManagedStreamConfig) -> Result<VirtualCamera> {
        #[cfg(windows)]
        {
            validate_stream(&stream)?;
            self.get(id)?;
            platform::validate_registered_driver(id)?;
        }
        #[cfg(not(windows))]
        self.get(id)?;
        VirtualCamera::open_managed(id, stream)
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.encode_utf16().count() > 255 || name.encode_utf16().any(|unit| unit == 0) {
        return Err(Error::InvalidCameraName);
    }
    Ok(())
}

#[cfg(windows)]
fn validate_stream(stream: &ManagedStreamConfig) -> Result<()> {
    if stream.width == 0 || stream.height == 0 || !stream.width.is_multiple_of(2) || !stream.height.is_multiple_of(2) || stream.width > i32::MAX as u32 || stream.height > i32::MAX as u32 {
        return Err(Error::InvalidDimensions);
    }
    let pixels = u64::from(stream.width) * u64::from(stream.height);
    if pixels > (i32::MAX as u64) / 12 {
        return Err(Error::InvalidDimensions);
    }
    if stream.frame_rate.numerator == 0 || stream.frame_rate.denominator == 0 {
        return Err(Error::InvalidFrameRate);
    }
    let interval = u128::from(stream.frame_rate.denominator) * 10_000_000 / u128::from(stream.frame_rate.numerator);
    if interval == 0 || interval > i64::MAX as u128 {
        return Err(Error::InvalidFrameRate);
    }
    Ok(())
}

#[cfg(windows)]
mod platform {
    use core::ffi::c_void;
    use core::mem;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

    use super::{ManagedCameraConfig, ManagedCameraId, ManagedCameraInfo};
    use crate::{Error, Result};

    const NAME_CAPACITY: usize = 256;
    const ABI_VERSION: u32 = 2;

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

    fn wide_string(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn registered_driver_path(id: ManagedCameraId) -> Result<PathBuf> {
        let key_path = wide_string(&format!("CLSID\\{id}\\InprocServer32"));
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
        let name = wide_string(name);
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
    fn managed_registration_only_validates_identity() {
        let config = ManagedCameraConfig::default();
        assert!(validate_name(&config.name).is_ok());
        assert_eq!(validate_name(""), Err(Error::InvalidCameraName));
    }

    #[test]
    fn managed_stream_defaults_to_nv12_friendly_dimensions() {
        let stream = ManagedStreamConfig::default();
        assert_eq!((stream.width, stream.height), (1920, 1080));
        assert_eq!(stream.frame_rate, ObsFrameRate::default());
    }

    #[cfg(windows)]
    #[test]
    fn managed_stream_validation_matches_native_fixed_mode_limits() {
        let stream = ManagedStreamConfig::default();
        assert!(validate_stream(&stream).is_ok());
        assert_eq!(validate_stream(&ManagedStreamConfig { width: 1919, ..stream }), Err(Error::InvalidDimensions));
        assert_eq!(validate_stream(&ManagedStreamConfig { width: 65_536, height: 65_536, ..stream }), Err(Error::InvalidDimensions));
        assert_eq!(
            validate_stream(&ManagedStreamConfig {
                frame_rate: ObsFrameRate { numerator: 0, denominator: 1 },
                ..stream
            }),
            Err(Error::InvalidFrameRate)
        );
    }
}
