//! Source configuration decoded from untrusted vendor attributes.

use virtualcam_protocol::mf::{ATTR_CAMERA_ID, ATTR_EXPECTED_OWNER_SID, ATTR_NATIVE_FORMAT, ATTR_PROTOCOL_VERSION, ATTR_RENDEZVOUS_ID, MAX_STRING_ATTRIBUTE, PROTOCOL_VERSION, is_valid_rendezvous_id};
use virtualcam_protocol::{CameraId, ValidatedFormat, WireVideoFormat};
use windows::Win32::Media::MediaFoundation::{IMFAttributes, MF_E_INVALIDREQUEST};
use windows_core::GUID;

use crate::security::Sid;

/// Everything the source needs; immutable for the source's lifetime.
#[derive(Clone, Debug)]
pub(crate) struct SourceConfig {
    pub(crate) camera_id: CameraId,
    pub(crate) format: ValidatedFormat,
    /// Producer rendezvous; `None` (fail closed: black only) when the
    /// routing id or owner assertion is missing or malformed.
    pub(crate) rendezvous: Option<Rendezvous>,
}

#[derive(Clone, Debug)]
pub(crate) struct Rendezvous {
    pub(crate) id: String,
    pub(crate) expected_owner: Sid,
}

fn key(id: CameraId) -> GUID {
    GUID::from_u128(id.to_u128())
}

/// Reads a bounded string attribute.
fn string(attributes: &IMFAttributes, id: CameraId) -> Option<String> {
    // SAFETY: Valid attribute store; the buffer is sized from the bounded length.
    let length = unsafe { attributes.GetStringLength(&key(id)) }.ok()? as usize;
    if length == 0 || length > MAX_STRING_ATTRIBUTE {
        return None;
    }
    let mut buffer = vec![0_u16; length + 1];
    // SAFETY: `buffer` holds `length + 1` units including the terminator.
    unsafe { attributes.GetString(&key(id), &mut buffer, None) }.ok()?;
    String::from_utf16(&buffer[..length]).ok()
}

impl SourceConfig {
    /// Validates the camera's attributes. A missing/invalid id, version, or
    /// native mode fails activation; a missing rendezvous only disables
    /// producer attach.
    pub(crate) fn from_attributes(attributes: &IMFAttributes) -> windows_core::Result<Self> {
        let invalid = || windows_core::Error::from(MF_E_INVALIDREQUEST);
        // SAFETY: Valid attribute store.
        if unsafe { attributes.GetUINT32(&key(ATTR_PROTOCOL_VERSION)) }.ok() != Some(PROTOCOL_VERSION) {
            return Err(invalid());
        }
        let camera_id: CameraId = string(attributes, ATTR_CAMERA_ID).and_then(|text| text.parse().ok()).filter(|id: &CameraId| !id.is_nil()).ok_or_else(invalid)?;
        let mut blob = [0_u8; WireVideoFormat::SIZE];
        let mut size = 0_u32;
        // SAFETY: `blob` has the exact wire size; larger blobs fail the call.
        unsafe { attributes.GetBlob(&key(ATTR_NATIVE_FORMAT), &mut blob, Some(&mut size)) }.map_err(|_| invalid())?;
        if size as usize != WireVideoFormat::SIZE {
            return Err(invalid());
        }
        let format = WireVideoFormat::from_bytes(&blob).and_then(WireVideoFormat::decode).and_then(|format| format.validate()).map_err(|_| invalid())?;
        let rendezvous = match (string(attributes, ATTR_RENDEZVOUS_ID), string(attributes, ATTR_EXPECTED_OWNER_SID).and_then(|text| Sid::parse(&text))) {
            (Some(id), Some(expected_owner)) if is_valid_rendezvous_id(&id) => Some(Rendezvous { id, expected_owner }),
            _ => None,
        };
        Ok(Self { camera_id, format, rendezvous })
    }
}
