//! Independently registered ("managed") virtual cameras.
//!
//! The current managed backend is DirectShow: registration persists only the
//! device identity, and each producer session chooses its NV12 runtime mode.
//! A future Media Foundation backend fixes one native mode at registration
//! instead, so its registration API will differ deliberately rather than being
//! folded into [`ManagedCameraConfig`].

use core::fmt;
use core::str::FromStr;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use crate::config::Nv12Format;
use crate::{Error, FrameRate, Result, VirtualCamera};

#[cfg(windows)]
mod native_driver;

/// Stable identifier of one managed DirectShow virtual-camera registration.
///
/// The value is the device's COM CLSID. It can be persisted as the canonical
/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}` string returned by [`Display`](fmt::Display).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ManagedCameraId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
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

/// Runtime format selected for one managed producer session.
///
/// A DirectShow graph keeps the media type it negotiated when it opened. If that
/// graph opened before any producer, it uses the last successfully published
/// mode for this CLSID and Windows user (or 640x480@30 if no valid history exists).
/// It can attach without reconnecting only to a later producer using that mode.
/// Reopening the consumer while a producer is live selects that session's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManagedStreamConfig {
    /// NV12 width. Must be non-zero and even.
    pub width: u32,
    /// NV12 height. Must be non-zero and even.
    pub height: u32,
    /// Frame rate advertised for this producer session.
    pub frame_rate: FrameRate,
}

impl Default for ManagedStreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frame_rate: FrameRate::default(),
        }
    }
}

#[cfg(windows)]
impl ManagedStreamConfig {
    /// Validates the stream against the NV12 queue and the native DirectShow
    /// source's media-type limits.
    pub(crate) fn validate(self) -> Result<Nv12Format> {
        let format = Nv12Format::new(self.width, self.height, self.frame_rate)?;
        if self.width > i32::MAX as u32 || self.height > i32::MAX as u32 {
            return Err(Error::InvalidDimensions);
        }
        // The pinned libdshowcapture media type computes `cx * cy * 12` in
        // signed `int` arithmetic; keep that product representable.
        let pixels = u64::from(self.width) * u64::from(self.height);
        if pixels > (i32::MAX as u64) / 12 {
            return Err(Error::InvalidDimensions);
        }
        // `REFERENCE_TIME` is signed, and the source converts to nanoseconds.
        if format.interval_100ns > i64::MAX as u64 || format.interval_100ns > u64::MAX / 100 {
            return Err(Error::InvalidFrameRate);
        }
        Ok(format)
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

/// Management facade for the managed DirectShow camera driver DLL.
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
    /// No stream format is persisted. [`Self::open`] publishes each producer
    /// session's runtime format without changing the registered device identity.
    pub fn register(&self, config: ManagedCameraConfig) -> Result<ManagedCameraInfo> {
        validate_name(&config.name)?;
        native_driver::register(&self.driver_path, &config.name)
    }

    /// Removes exactly one camera registration. Other managed cameras and OBS
    /// Virtual Camera are unaffected.
    pub fn unregister(&self, id: ManagedCameraId) -> Result<()> {
        native_driver::unregister(&self.driver_path, id)
    }

    /// Changes one camera's DirectShow friendly name without changing its CLSID
    /// or shared-memory queue identity.
    pub fn rename(&self, id: ManagedCameraId, name: impl AsRef<str>) -> Result<()> {
        validate_name(name.as_ref())?;
        native_driver::rename(&self.driver_path, id, name.as_ref())
    }

    /// Returns all camera registrations owned by this driver family.
    pub fn list(&self) -> Result<Vec<ManagedCameraInfo>> {
        native_driver::list(&self.driver_path)
    }

    /// Reads one registration by CLSID.
    pub fn get(&self, id: ManagedCameraId) -> Result<ManagedCameraInfo> {
        native_driver::get(&self.driver_path, id)
    }

    /// Opens one producer session with the runtime NV12 format to advertise.
    ///
    /// Managed ABI v3 publishes the format through a fixed control mapping and
    /// writes frames to a format-specific OBS-compatible queue. A consumer that
    /// is already connected keeps its negotiated mode; reopen that consumer to
    /// select a different live producer format. Later sessions may use another
    /// format with the same CLSID even while an old graph retains its prior data
    /// mapping.
    pub fn open(&self, id: ManagedCameraId, stream: ManagedStreamConfig) -> Result<VirtualCamera> {
        #[cfg(windows)]
        {
            let format = stream.validate()?;
            self.get(id)?;
            native_driver::validate_registered_driver(id)?;
            VirtualCamera::open_managed_dshow(id, format)
        }
        #[cfg(not(windows))]
        {
            let _ = (id, stream);
            Err(Error::UnsupportedPlatform)
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidCameraName);
    }
    let mut units = 0_usize;
    for unit in name.encode_utf16() {
        if unit == 0 {
            return Err(Error::InvalidCameraName);
        }
        units += 1;
        if units > 255 {
            return Err(Error::InvalidCameraName);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
mod native_driver {
    use std::path::Path;

    use super::{ManagedCameraId, ManagedCameraInfo};
    use crate::{Error, Result};

    pub(super) fn register(_: &Path, _: &str) -> Result<ManagedCameraInfo> {
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
        assert_eq!(validate_name("a\0b"), Err(Error::InvalidCameraName));
        assert!(validate_name(&"n".repeat(255)).is_ok());
        assert_eq!(validate_name(&"n".repeat(256)), Err(Error::InvalidCameraName));
    }

    #[test]
    fn managed_stream_defaults_to_nv12_friendly_dimensions() {
        let stream = ManagedStreamConfig::default();
        assert_eq!((stream.width, stream.height), (1920, 1080));
        assert_eq!(stream.frame_rate, FrameRate::default());
    }

    #[cfg(windows)]
    #[test]
    fn managed_stream_validation_matches_native_fixed_mode_limits() {
        let stream = ManagedStreamConfig::default();
        assert_eq!(stream.validate().map(|format| (format.width, format.height, format.interval_100ns)), Ok((1920, 1080, 333_333)));
        assert!(matches!(ManagedStreamConfig { width: 1919, ..stream }.validate(), Err(Error::InvalidDimensions)));
        assert!(matches!(ManagedStreamConfig { width: 65_536, height: 65_536, ..stream }.validate(), Err(Error::InvalidDimensions)));
        assert!(matches!(
            ManagedStreamConfig {
                frame_rate: FrameRate { numerator: 0, denominator: 1 },
                ..stream
            }
            .validate(),
            Err(Error::InvalidFrameRate)
        ));
    }
}
