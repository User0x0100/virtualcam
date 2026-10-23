//! Media Foundation camera management (implemented with the MF source).

use virtualcam_protocol::ValidatedFormat;

use super::metadata::CameraRecord;
use super::{AppId, MediaFoundationRegistration};
use crate::{CameraType, Error, Result, VirtualCamera};

pub(super) fn register(_app_id: Option<AppId>, _request: MediaFoundationRegistration) -> Result<CameraRecord> {
    Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation))
}

pub(super) fn unregister(_record: &CameraRecord) -> Result<()> {
    Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation))
}

pub(super) fn rename(_record: &CameraRecord, _name: &str) -> Result<CameraRecord> {
    Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation))
}

pub(super) fn health(_record: &CameraRecord) -> core::result::Result<(), &'static str> {
    Err("Media Foundation backend is not available")
}

pub(super) fn open(_record: &CameraRecord, _format: ValidatedFormat) -> Result<VirtualCamera> {
    Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation))
}
