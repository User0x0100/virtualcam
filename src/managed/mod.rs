//! Independently registered ("managed") virtual cameras.
//!
//! Two backends with deliberately different registration semantics:
//!
//! - **DirectShow**: registration persists only identity (plus an optional
//!   idle default format). Each producer session chooses its own format.
//!   Per-user registration needs no elevation; machine-wide needs admin.
//! - **Media Foundation**: registration fixes one immutable native mode,
//!   because Frame Server must enumerate media types before any producer
//!   exists. Cameras are per-user (`CurrentUser` access, `System` lifetime).
//!
//! Every camera may carry an [`AppId`] tag. A manager created with
//! [`ManagedCameraManager::for_app`] stamps its tag on new cameras and only
//! sees cameras carrying the same tag. The tag is a convenience namespace,
//! not a security boundary.

use core::fmt;
use std::path::PathBuf;

#[cfg(windows)]
pub use legacy::LegacyCamera;
pub use virtualcam_protocol::CameraId as ManagedCameraId;

use crate::{Error, PixelFormat, Result, VideoFormat, VirtualCamera};

#[cfg(windows)]
mod com;
#[cfg(windows)]
mod dshow;
#[cfg(windows)]
mod legacy;
#[cfg(windows)]
pub(crate) mod metadata;
#[cfg(windows)]
mod mf;

/// Application tag that groups the managed cameras one program registered.
///
/// 1–128 ASCII characters from `A-Z a-z 0-9 . _ -`, e.g. `com.example.studio`.
/// Comparison is exact (case-sensitive).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AppId(String);

impl AppId {
    /// Longest accepted id.
    pub const MAX_LEN: usize = 128;

    /// Validates and wraps an application id.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = !value.is_empty() && value.len() <= Self::MAX_LEN && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if valid { Ok(Self(value)) } else { Err(Error::InvalidAppId) }
    }

    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl core::str::FromStr for AppId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

/// Backend implementing a managed camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ManagedBackend {
    /// In-process DirectShow source filter (`virtualcam_dshow.dll`).
    DirectShow,
    /// Frame Server custom media source (`virtualcam_mf.dll`).
    MediaFoundation,
}

/// Who can see a managed camera.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RegistrationScope {
    /// Only the registering Windows user. Needs no elevation for DirectShow.
    #[default]
    CurrentUser,
    /// Every user of the machine. DirectShow only; requires elevation.
    AllUsers,
}

/// Registration request for a DirectShow camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectShowRegistration {
    /// Friendly name shown by camera enumerators.
    pub name: String,
    /// Visibility of the camera.
    pub scope: RegistrationScope,
    /// Format advertised while no producer runs. `None` falls back to the
    /// user's last published format, then NV12 640x480@30.
    pub default_format: Option<VideoFormat>,
    /// Pixel formats offered beside the current one, in preference order.
    /// The connected graph's choice reaches the producer through
    /// [`VirtualCamera::requested_format`](crate::VirtualCamera::requested_format);
    /// list only formats the producer will switch to. Empty offers just the
    /// current format.
    pub pixel_formats: Vec<PixelFormat>,
}

impl DirectShowRegistration {
    /// A per-user camera without an idle default format.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            scope: RegistrationScope::CurrentUser,
            default_format: None,
            pixel_formats: Vec::new(),
        }
    }
}

/// Registration request for a Media Foundation camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaFoundationRegistration {
    /// Friendly name; unique among this user's Media Foundation cameras.
    pub name: String,
    /// The single immutable mode the camera advertises and producers write.
    pub native_format: VideoFormat,
}

impl MediaFoundationRegistration {
    /// A camera with the given immutable native mode.
    pub fn new(name: impl Into<String>, native_format: VideoFormat) -> Self {
        Self { name: name.into(), native_format }
    }
}

/// Backend-specific registration request.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagedCameraRegistration {
    /// See [`DirectShowRegistration`].
    DirectShow(DirectShowRegistration),
    /// See [`MediaFoundationRegistration`].
    MediaFoundation(MediaFoundationRegistration),
}

impl From<DirectShowRegistration> for ManagedCameraRegistration {
    fn from(value: DirectShowRegistration) -> Self {
        Self::DirectShow(value)
    }
}

impl From<MediaFoundationRegistration> for ManagedCameraRegistration {
    fn from(value: MediaFoundationRegistration) -> Self {
        Self::MediaFoundation(value)
    }
}

/// Whether every part of a registration is present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationHealth {
    /// Metadata, OS registration, and source binary are all present.
    Healthy,
    /// Some part is missing; unregister and register again to repair.
    Degraded(&'static str),
}

/// Persistent metadata of one managed camera.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedCameraInfo {
    /// Stable id (the DirectShow CLSID / Media Foundation routing id).
    pub id: ManagedCameraId,
    /// Friendly name shown to camera consumers.
    pub name: String,
    /// Implementing backend.
    pub backend: ManagedBackend,
    /// Visibility; always [`RegistrationScope::CurrentUser`] for Media Foundation.
    pub scope: RegistrationScope,
    /// Owning application tag, if any.
    pub app_id: Option<AppId>,
    /// DirectShow idle default format, or the Media Foundation native mode.
    pub format: Option<VideoFormat>,
    /// DirectShow offered pixel formats (see
    /// [`DirectShowRegistration::pixel_formats`]); empty for Media Foundation.
    pub pixel_formats: Vec<PixelFormat>,
    /// Registration completeness.
    pub health: RegistrationHealth,
}

/// Registers, inspects, and opens managed cameras.
///
/// Cheap to clone. All mutations are serialized across processes by a
/// machine-wide management mutex.
#[derive(Clone, Debug, Default)]
pub struct ManagedCameraManager {
    app_id: Option<AppId>,
    dshow_source: Option<PathBuf>,
}

impl ManagedCameraManager {
    /// A manager that sees every managed camera of the current user plus all
    /// machine-wide cameras.
    pub fn new() -> Self {
        Self::default()
    }

    /// A manager scoped to one application: new cameras are tagged with
    /// `app_id`, and list/get/open/rename/unregister only see cameras with
    /// that tag (others report [`Error::CameraNotFound`]).
    pub fn for_app(app_id: AppId) -> Self {
        Self { app_id: Some(app_id), ..Self::default() }
    }

    /// Overrides the DirectShow source DLL registered for new cameras.
    /// Defaults to `virtualcam_dshow.dll` next to the current executable.
    pub fn with_dshow_source(mut self, path: impl Into<PathBuf>) -> Self {
        self.dshow_source = Some(path.into());
        self
    }

    /// The application scope, if any.
    pub fn app_id(&self) -> Option<&AppId> {
        self.app_id.as_ref()
    }

    /// Registers a new camera and returns its metadata.
    pub fn register(&self, registration: impl Into<ManagedCameraRegistration>) -> Result<ManagedCameraInfo> {
        let registration = registration.into();
        match &registration {
            ManagedCameraRegistration::DirectShow(r) => {
                validate_name(&r.name)?;
                if let Some(format) = r.default_format {
                    format.validate()?;
                }
                if r.pixel_formats.iter().any(|format| !format.is_managed()) || r.pixel_formats.len() > virtualcam_protocol::registry::MAX_PIXEL_FORMATS {
                    return Err(virtualcam_protocol::FormatError::UnsupportedPixelFormat.into());
                }
            }
            ManagedCameraRegistration::MediaFoundation(r) => {
                validate_name(&r.name)?;
                r.native_format.validate()?;
            }
        }
        imp::register(self, registration)
    }

    /// Permanently removes one camera. Other cameras are unaffected.
    pub fn unregister(&self, id: ManagedCameraId) -> Result<()> {
        imp::unregister(self, id)
    }

    /// Renames one camera. DirectShow renames in place; Media Foundation
    /// recreates the system camera (its symbolic link changes, its id does not).
    pub fn rename(&self, id: ManagedCameraId, name: impl AsRef<str>) -> Result<ManagedCameraInfo> {
        validate_name(name.as_ref())?;
        imp::rename(self, id, name.as_ref())
    }

    /// Lists cameras visible to this manager, sorted by name.
    pub fn list(&self) -> Result<Vec<ManagedCameraInfo>> {
        imp::list(self)
    }

    /// Reads one camera.
    pub fn get(&self, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
        imp::get(self, id)
    }

    /// Opens a producer session.
    ///
    /// DirectShow accepts any valid managed format per session; a consumer
    /// graph that is already connected keeps its negotiated format (and shows
    /// black for a different one) until it reconnects. Media Foundation
    /// requires `format` to equal the native mode exactly, otherwise
    /// [`Error::FormatMismatch`].
    pub fn open(&self, id: ManagedCameraId, format: VideoFormat) -> Result<VirtualCamera> {
        imp::open(self, id, Some(format))
    }

    /// Installs the shared Media Foundation source DLL machine-wide.
    ///
    /// Required once per machine before Media Foundation cameras can be
    /// registered; needs an elevated process. Frame Server loads the DLL as
    /// Local Service, so place it where that account can read it (for
    /// example under Program Files), never in a user profile.
    pub fn install_media_foundation_source(path: impl AsRef<std::path::Path>) -> Result<()> {
        #[cfg(windows)]
        {
            mf::install_source(path.as_ref())
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(Error::UnsupportedPlatform)
        }
    }

    /// Removes the machine-wide Media Foundation source registration.
    ///
    /// Fails safe while the current user still has Media Foundation
    /// cameras. Cameras of other Windows users cannot be enumerated here;
    /// pass `other_users_cleaned = true` only after they were removed (each
    /// user unregisters their own), otherwise the source stays installed.
    pub fn uninstall_media_foundation_source(other_users_cleaned: bool) -> Result<()> {
        #[cfg(windows)]
        {
            mf::uninstall_source(other_users_cleaned)
        }
        #[cfg(not(windows))]
        {
            let _ = other_users_cleaned;
            Err(Error::UnsupportedPlatform)
        }
    }

    /// Lists cameras registered by the retired C++ DirectShow driver.
    ///
    /// They keep working only while their C++ DLL exists. Migrate by
    /// removing each with [`Self::remove_legacy_camera`] and registering a
    /// new camera (the CLSID changes).
    #[cfg(windows)]
    pub fn legacy_cameras() -> Result<Vec<LegacyCamera>> {
        legacy::list()
    }

    /// Removes one legacy C++-driver camera (requires elevation).
    #[cfg(windows)]
    pub fn remove_legacy_camera(id: ManagedCameraId) -> Result<()> {
        let _lock = imp::ManagementLock::acquire()?;
        legacy::remove(id)
    }

    /// The installed Media Foundation source DLL, if any.
    pub fn media_foundation_source() -> Option<PathBuf> {
        #[cfg(windows)]
        {
            mf::installed_source()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Opens a producer session with the camera's registered format (the
    /// Media Foundation native mode or the DirectShow default format).
    pub fn open_registered(&self, id: ManagedCameraId) -> Result<VirtualCamera> {
        imp::open(self, id, None)
    }
}

fn validate_name(name: &str) -> Result<()> {
    let mut units = 0_usize;
    for unit in name.encode_utf16() {
        if unit < 0x20 || unit == 0x7F {
            return Err(Error::InvalidCameraName);
        }
        units += 1;
    }
    if units == 0 || units > 255 || name.trim() != name {
        return Err(Error::InvalidCameraName);
    }
    Ok(())
}

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};

    use virtualcam_protocol::{FrameRate, PixelFormat};
    use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

    use super::metadata::{CameraRecord, history_path};
    use super::*;
    use crate::CameraType;
    use crate::platform::registry::{Hive, Key};
    use crate::platform::windows::{OwnedHandle, random_bytes, wide_nul};

    /// Cross-process, cross-session serialization of every mutation.
    pub(super) struct ManagementLock(OwnedHandle);

    impl ManagementLock {
        pub(super) fn acquire() -> Result<Self> {
            let name = wide_nul("Global\\VirtualCamRs-Management");
            // SAFETY: NUL-terminated name, default security, not initially owned.
            let raw = unsafe { CreateMutexW(core::ptr::null(), 0, name.as_ptr()) };
            // SAFETY: A non-null CreateMutexW result is one owned handle.
            let handle = unsafe { OwnedHandle::from_result(raw, "CreateMutexW(management)") }?;
            // SAFETY: Live mutex handle with SYNCHRONIZE access.
            match unsafe { WaitForSingleObject(handle.raw(), 30_000) } {
                // An abandoned mutex means a manager died mid-mutation; every
                // mutation is individually rolled back, so continuing is safe.
                WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
                _ => Err(Error::Windows {
                    operation: "WaitForSingleObject(management mutex)",
                    code: windows_sys::Win32::Foundation::WAIT_TIMEOUT,
                }),
            }
        }
    }

    impl Drop for ManagementLock {
        fn drop(&mut self) {
            // SAFETY: This thread owns the mutex acquired in `acquire`.
            unsafe { ReleaseMutex(self.0.raw()) };
        }
    }

    const fn hive_of(scope: RegistrationScope) -> Hive {
        match scope {
            RegistrationScope::CurrentUser => Hive::CurrentUser,
            RegistrationScope::AllUsers => Hive::LocalMachine,
        }
    }

    const fn scope_of(hive: Hive) -> RegistrationScope {
        match hive {
            Hive::CurrentUser => RegistrationScope::CurrentUser,
            Hive::LocalMachine => RegistrationScope::AllUsers,
        }
    }

    fn visible(manager: &ManagedCameraManager, record: &CameraRecord) -> bool {
        manager.app_id.as_ref().is_none_or(|app| record.app_id.as_ref() == Some(app))
    }

    fn all_records() -> Result<Vec<CameraRecord>> {
        let mut records = CameraRecord::list(Hive::CurrentUser)?;
        records.extend(CameraRecord::list(Hive::LocalMachine)?);
        Ok(records)
    }

    fn find(manager: &ManagedCameraManager, id: ManagedCameraId) -> Result<CameraRecord> {
        for hive in [Hive::CurrentUser, Hive::LocalMachine] {
            if let Some(record) = CameraRecord::read(hive, id)?
                && visible(manager, &record)
            {
                return Ok(record);
            }
        }
        Err(Error::CameraNotFound)
    }

    fn health(record: &CameraRecord) -> RegistrationHealth {
        let result = match record.backend {
            ManagedBackend::DirectShow => dshow::health(record.hive, record.id, record.source_path.as_deref()),
            ManagedBackend::MediaFoundation => mf::health(record),
        };
        match result {
            Ok(()) => RegistrationHealth::Healthy,
            Err(detail) => RegistrationHealth::Degraded(detail),
        }
    }

    fn info(record: &CameraRecord) -> ManagedCameraInfo {
        ManagedCameraInfo {
            id: record.id,
            name: record.name.clone(),
            backend: record.backend,
            scope: scope_of(record.hive),
            app_id: record.app_id.clone(),
            format: record.format,
            pixel_formats: record.pixel_formats.clone(),
            health: health(record),
        }
    }

    fn check_name_free(backend: ManagedBackend, hive: Hive, name: &str, except: Option<ManagedCameraId>) -> Result<()> {
        let taken = CameraRecord::list(hive)?.into_iter().any(|r| r.backend == backend && Some(r.id) != except && r.name.eq_ignore_ascii_case(name));
        if taken { Err(Error::NameConflict) } else { Ok(()) }
    }

    pub(crate) fn new_camera_id() -> Result<ManagedCameraId> {
        loop {
            let id = ManagedCameraId::from_random_bytes(random_bytes()?);
            if !id.is_nil() && CameraRecord::read(Hive::CurrentUser, id)?.is_none() && CameraRecord::read(Hive::LocalMachine, id)?.is_none() {
                return Ok(id);
            }
        }
    }

    /// Resolves the DirectShow source to an absolute local path suitable for
    /// `InprocServer32`.
    fn dshow_source(manager: &ManagedCameraManager) -> Result<String> {
        let path = match &manager.dshow_source {
            Some(path) => path.clone(),
            None => std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("virtualcam_dshow.dll")))
                .unwrap_or_else(|| PathBuf::from("virtualcam_dshow.dll")),
        };
        let canonical = std::fs::canonicalize(&path).map_err(|_| Error::BackendUnavailable(CameraType::ManagedDirectShow))?;
        let text = canonical.to_str().ok_or(Error::RegistrationDegraded {
            detail: "source path is not valid Unicode",
        })?;
        // `canonicalize` yields verbatim paths; COM wants a plain DOS path,
        // and network shares are refused as in-process COM servers.
        let plain = match text.strip_prefix(r"\\?\") {
            Some(rest) if rest.starts_with("UNC\\") => return Err(Error::BackendUnavailable(CameraType::ManagedDirectShow)),
            Some(rest) => rest.to_owned(),
            None if text.starts_with(r"\\") => return Err(Error::BackendUnavailable(CameraType::ManagedDirectShow)),
            None => text.to_owned(),
        };
        if !Path::new(&plain).is_file() {
            return Err(Error::BackendUnavailable(CameraType::ManagedDirectShow));
        }
        Ok(plain)
    }

    pub(super) fn register(manager: &ManagedCameraManager, registration: ManagedCameraRegistration) -> Result<ManagedCameraInfo> {
        let _lock = ManagementLock::acquire()?;
        let record = match registration {
            ManagedCameraRegistration::DirectShow(request) => {
                let hive = hive_of(request.scope);
                check_name_free(ManagedBackend::DirectShow, hive, &request.name, None)?;
                let source = dshow_source(manager)?;
                let record = CameraRecord {
                    id: new_camera_id()?,
                    hive,
                    backend: ManagedBackend::DirectShow,
                    name: request.name,
                    app_id: manager.app_id.clone(),
                    format: request.default_format,
                    pixel_formats: request.pixel_formats,
                    source_path: Some(source.clone()),
                    rendezvous_id: None,
                    symbolic_link: None,
                };
                dshow::register(hive, record.id, &record.name, Path::new(&source))?;
                if let Err(error) = record.write() {
                    let _ = dshow::unregister(hive, record.id);
                    return Err(error);
                }
                record
            }
            ManagedCameraRegistration::MediaFoundation(request) => {
                check_name_free(ManagedBackend::MediaFoundation, Hive::CurrentUser, &request.name, None)?;
                mf::register(manager.app_id.clone(), request)?
            }
        };
        Ok(info(&record))
    }

    pub(super) fn unregister(manager: &ManagedCameraManager, id: ManagedCameraId) -> Result<()> {
        let _lock = ManagementLock::acquire()?;
        let record = find(manager, id)?;
        match record.backend {
            ManagedBackend::DirectShow => dshow::unregister(record.hive, id)?,
            ManagedBackend::MediaFoundation => mf::unregister(&record)?,
        }
        CameraRecord::delete(record.hive, id)?;
        let _ = Key::delete_tree(Hive::CurrentUser, &history_path(id));
        Ok(())
    }

    pub(super) fn rename(manager: &ManagedCameraManager, id: ManagedCameraId, name: &str) -> Result<ManagedCameraInfo> {
        let _lock = ManagementLock::acquire()?;
        let record = find(manager, id)?;
        check_name_free(record.backend, record.hive, name, Some(id))?;
        let record = match record.backend {
            ManagedBackend::DirectShow => {
                dshow::rename(record.hive, id, name)?;
                if let Err(error) = record.set_name(name) {
                    let _ = dshow::rename(record.hive, id, &record.name);
                    return Err(error);
                }
                CameraRecord { name: name.to_owned(), ..record }
            }
            ManagedBackend::MediaFoundation => mf::rename(&record, name)?,
        };
        Ok(info(&record))
    }

    pub(super) fn list(manager: &ManagedCameraManager) -> Result<Vec<ManagedCameraInfo>> {
        Ok(all_records()?.iter().filter(|record| visible(manager, record)).map(info).collect())
    }

    pub(super) fn get(manager: &ManagedCameraManager, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
        find(manager, id).map(|record| info(&record))
    }

    pub(super) fn open(manager: &ManagedCameraManager, id: ManagedCameraId, format: Option<VideoFormat>) -> Result<VirtualCamera> {
        let record = find(manager, id)?;
        if let RegistrationHealth::Degraded(detail) = health(&record) {
            return Err(Error::RegistrationDegraded { detail });
        }
        match record.backend {
            ManagedBackend::DirectShow => {
                let format = format.or(record.format).unwrap_or_else(|| VideoFormat::new(PixelFormat::Nv12, 640, 480, FrameRate::fps(30)));
                VirtualCamera::open_managed_dshow(id, format.validate()?)
            }
            ManagedBackend::MediaFoundation => {
                let native = record.format.ok_or(Error::RegistrationDegraded { detail: "native format is missing" })?;
                if format.is_some_and(|format| format != native) {
                    return Err(Error::FormatMismatch);
                }
                mf::open(&record, native.validate()?)
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub(super) fn register(_: &ManagedCameraManager, _: ManagedCameraRegistration) -> Result<ManagedCameraInfo> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn unregister(_: &ManagedCameraManager, _: ManagedCameraId) -> Result<()> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn rename(_: &ManagedCameraManager, _: ManagedCameraId, _: &str) -> Result<ManagedCameraInfo> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn list(_: &ManagedCameraManager) -> Result<Vec<ManagedCameraInfo>> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn get(_: &ManagedCameraManager, _: ManagedCameraId) -> Result<ManagedCameraInfo> {
        Err(Error::UnsupportedPlatform)
    }
    pub(super) fn open(_: &ManagedCameraManager, _: ManagedCameraId, _: Option<VideoFormat>) -> Result<VirtualCamera> {
        Err(Error::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_restricted_ascii() {
        assert_eq!(AppId::new("com.example.studio-2_b").map(|id| id.to_string()), Ok("com.example.studio-2_b".to_owned()));
        for bad in ["", "has space", "slash/es", "ünicode", &"a".repeat(129)] {
            assert_eq!(AppId::new(bad), Err(Error::InvalidAppId), "{bad}");
        }
        assert!(AppId::new("a".repeat(128)).is_ok());
    }

    #[test]
    fn names_reject_control_characters_and_padding() {
        assert!(validate_name("Studio Cam ✓").is_ok());
        assert!(validate_name(&"n".repeat(255)).is_ok());
        for bad in ["", " lead", "trail ", "a\0b", "tab\there", &"n".repeat(256)] {
            assert_eq!(validate_name(bad), Err(Error::InvalidCameraName), "{bad:?}");
        }
    }

    #[test]
    fn scoped_manager_carries_app_id() {
        let manager = ManagedCameraManager::for_app(AppId::new("com.example").expect("valid"));
        assert_eq!(manager.app_id().map(AppId::as_str), Some("com.example"));
        assert_eq!(ManagedCameraManager::new().app_id(), None);
    }
}
