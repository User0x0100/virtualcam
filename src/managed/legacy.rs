//! Cleanup of cameras registered by the retired C++ DirectShow driver.
//!
//! The C++ driver (managed ABI v3) recorded cameras machine-wide under
//! `HKLM\SOFTWARE\VirtualCamRs\ManagedCameras\{CLSID}` and registered each
//! CLSID as a COM class plus a video-input category instance in HKLM. Its
//! runtime transport is incompatible with the Rust sources, so migration is
//! deliberately breaking: remove the legacy camera (elevated) while its DLL
//! still exists, then register a new camera.

use std::path::PathBuf;

use crate::platform::registry::{Hive, Key};
use crate::{Error, ManagedCameraId, Result};

const LEGACY_ROOT: &str = "SOFTWARE\\VirtualCamRs\\ManagedCameras";
const LEGACY_HISTORY: &str = "Software\\VirtualCamRs\\ManagedCameraFormats";

/// A camera registered by the retired C++ DirectShow driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyCamera {
    /// Its COM CLSID.
    pub id: ManagedCameraId,
    /// Its friendly name.
    pub name: String,
    /// The C++ DLL its COM class still points at, if registered.
    pub driver_path: Option<PathBuf>,
}

pub(super) fn list() -> Result<Vec<LegacyCamera>> {
    let Some(root) = Key::open(Hive::LocalMachine, LEGACY_ROOT)? else {
        return Ok(Vec::new());
    };
    let mut cameras = Vec::new();
    for subkey in root.subkeys()? {
        let Ok(id) = subkey.parse::<ManagedCameraId>() else { continue };
        let name = Key::open(Hive::LocalMachine, &format!("{LEGACY_ROOT}\\{subkey}"))?.and_then(|key| key.get_string(Some("Name")).ok().flatten()).unwrap_or_default();
        let driver_path = Key::open(Hive::LocalMachine, &format!("Software\\Classes\\CLSID\\{id}\\InprocServer32"))?.and_then(|key| key.get_string(None).ok().flatten()).map(PathBuf::from);
        cameras.push(LegacyCamera { id, name, driver_path });
    }
    Ok(cameras)
}

/// Removes one legacy camera's category instance, COM class, and record.
/// Requires elevation.
pub(super) fn remove(id: ManagedCameraId) -> Result<()> {
    if !Key::exists(Hive::LocalMachine, &format!("{LEGACY_ROOT}\\{id}")) {
        return Err(Error::CameraNotFound);
    }
    super::dshow::unregister(Hive::LocalMachine, id)?;
    Key::delete_tree(Hive::LocalMachine, &format!("{LEGACY_ROOT}\\{id}"))?;
    let _ = Key::delete_tree(Hive::CurrentUser, &format!("{LEGACY_HISTORY}\\{id}"));
    Ok(())
}
