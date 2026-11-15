//! Camera records in `{HKCU|HKLM}\Software\VirtualCamRs\Cameras\{ID}`.

use virtualcam_protocol::registry::{BACKEND_DIRECTSHOW, BACKEND_MEDIA_FOUNDATION, CAMERAS_KEY, HISTORY_KEY, decode_frame_rates, decode_pixel_formats, encode_frame_rates, encode_pixel_formats, values};
use virtualcam_protocol::{FrameRate, PixelFormat, VideoFormat, WireVideoFormat};

use super::{AppId, ManagedBackend};
use crate::platform::registry::{Hive, Key};
use crate::{ManagedCameraId, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CameraRecord {
    pub(crate) id: ManagedCameraId,
    pub(crate) hive: Hive,
    pub(crate) backend: ManagedBackend,
    pub(crate) name: String,
    pub(crate) app_id: Option<AppId>,
    /// DirectShow idle default format, or the Media Foundation default format.
    pub(crate) format: Option<VideoFormat>,
    /// Offered pixel formats in declaration order: DirectShow's advertised
    /// set, or the Media Foundation declared format set.
    pub(crate) pixel_formats: Vec<PixelFormat>,
    pub(crate) frame_rates: Vec<FrameRate>,
    pub(crate) source_path: Option<String>,
    pub(crate) rendezvous_id: Option<String>,
    pub(crate) symbolic_link: Option<String>,
}

fn key_path(id: ManagedCameraId) -> String {
    format!("{CAMERAS_KEY}\\{id}")
}

pub(crate) fn history_path(id: ManagedCameraId) -> String {
    format!("{HISTORY_KEY}\\{id}")
}

impl CameraRecord {
    /// Writes every value; on failure the partially written key is removed.
    pub(crate) fn write(&self) -> Result<()> {
        let result = (|| {
            let key = Key::create(self.hive, &key_path(self.id))?;
            key.set_u32(
                values::BACKEND,
                match self.backend {
                    ManagedBackend::DirectShow => BACKEND_DIRECTSHOW,
                    ManagedBackend::MediaFoundation => BACKEND_MEDIA_FOUNDATION,
                },
            )?;
            key.set_string(Some(values::NAME), &self.name)?;
            if let Some(app_id) = &self.app_id {
                key.set_string(Some(values::APP_ID), app_id.as_str())?;
            }
            if let Some(format) = self.format {
                key.set_binary(values::FORMAT, &format.to_wire()?.to_bytes())?;
            }
            if !self.pixel_formats.is_empty() {
                key.set_binary(values::PIXEL_FORMATS, &encode_pixel_formats(&self.pixel_formats))?;
            }
            if !self.frame_rates.is_empty() {
                key.set_binary(values::FRAME_RATES, &encode_frame_rates(&self.frame_rates))?;
            }
            for (name, value) in [(values::SOURCE_PATH, &self.source_path), (values::RENDEZVOUS_ID, &self.rendezvous_id), (values::SYMBOLIC_LINK, &self.symbolic_link)] {
                if let Some(value) = value {
                    key.set_string(Some(name), value)?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = Self::delete(self.hive, self.id);
        }
        result
    }

    /// Reads one record. Unknown backends and unparsable keys yield `None`
    /// (they are not ours to interpret); an invalid stored format is dropped.
    pub(crate) fn read(hive: Hive, id: ManagedCameraId) -> Result<Option<Self>> {
        let Some(key) = Key::open(hive, &key_path(id))? else {
            return Ok(None);
        };
        let backend = match key.get_u32(values::BACKEND)? {
            Some(BACKEND_DIRECTSHOW) => ManagedBackend::DirectShow,
            Some(BACKEND_MEDIA_FOUNDATION) => ManagedBackend::MediaFoundation,
            _ => return Ok(None),
        };
        let format = key
            .get_binary(values::FORMAT)?
            .and_then(|bytes| WireVideoFormat::from_bytes(&bytes).and_then(WireVideoFormat::decode).ok())
            .filter(|format| format.validate().is_ok());
        Ok(Some(Self {
            id,
            hive,
            backend,
            name: key.get_string(Some(values::NAME))?.unwrap_or_default(),
            app_id: key.get_string(Some(values::APP_ID))?.and_then(|value| AppId::new(value).ok()),
            format,
            pixel_formats: key.get_binary(values::PIXEL_FORMATS)?.map(|bytes| decode_pixel_formats(&bytes)).unwrap_or_default(),
            frame_rates: key.get_binary(values::FRAME_RATES)?.map(|bytes| decode_frame_rates(&bytes)).unwrap_or_default(),
            source_path: key.get_string(Some(values::SOURCE_PATH))?,
            rendezvous_id: key.get_string(Some(values::RENDEZVOUS_ID))?,
            symbolic_link: key.get_string(Some(values::SYMBOLIC_LINK))?,
        }))
    }

    /// Lists every parsable record in `hive`.
    pub(crate) fn list(hive: Hive) -> Result<Vec<Self>> {
        let Some(root) = Key::open(hive, CAMERAS_KEY)? else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for name in root.subkeys()? {
            if let Ok(id) = name.parse::<ManagedCameraId>()
                && let Some(record) = Self::read(hive, id)?
            {
                records.push(record);
            }
        }
        records.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(records)
    }

    pub(crate) fn set_name(&self, name: &str) -> Result<()> {
        Key::create(self.hive, &key_path(self.id))?.set_string(Some(values::NAME), name)
    }

    pub(crate) fn delete(hive: Hive, id: ManagedCameraId) -> Result<()> {
        Key::delete_tree(hive, &key_path(id))
    }
}
