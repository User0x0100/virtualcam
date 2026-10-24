//! Camera configuration read from the manager's registry records.

use virtualcam_protocol::registry::{BACKEND_DIRECTSHOW, CAMERAS_KEY, HISTORY_KEY, MAX_PIXEL_FORMATS, decode_pixel_formats, values};
use virtualcam_protocol::{CameraId, FrameRate, PixelFormat, VideoFormat, WireVideoFormat};

use crate::win::{Hive, reg_binary, reg_string, reg_u32};

/// Format advertised when neither a producer, history, nor a default exists.
pub(crate) const FALLBACK_FORMAT: VideoFormat = VideoFormat::new(PixelFormat::Nv12, 640, 480, FrameRate::fps(30));

#[derive(Clone, Debug)]
pub(crate) struct CameraConfig {
    pub(crate) id: CameraId,
    pub(crate) name: String,
    pub(crate) default_format: Option<VideoFormat>,
    /// Registered pixel formats offered beside the current one; empty for
    /// cameras registered before multi-format support.
    pub(crate) pixel_formats: Vec<PixelFormat>,
}

fn decode_format(bytes: Option<Vec<u8>>) -> Option<VideoFormat> {
    let format = WireVideoFormat::from_bytes(&bytes?).ok()?.decode().ok()?;
    format.validate().ok().map(|_| format)
}

impl CameraConfig {
    /// Loads a DirectShow camera record (per-user first, then machine-wide).
    pub(crate) fn load(id: CameraId) -> Option<Self> {
        if id.is_nil() {
            return None;
        }
        let key = format!("{CAMERAS_KEY}\\{id}");
        [Hive::CurrentUser, Hive::LocalMachine].into_iter().find_map(|hive| {
            (reg_u32(hive, &key, values::BACKEND)? == BACKEND_DIRECTSHOW).then(|| Self {
                id,
                name: reg_string(hive, &key, values::NAME).unwrap_or_else(|| "VirtualCam".to_owned()),
                default_format: decode_format(reg_binary(hive, &key, values::FORMAT, WireVideoFormat::SIZE)),
                pixel_formats: reg_binary(hive, &key, values::PIXEL_FORMATS, MAX_PIXEL_FORMATS * 4).map(|bytes| decode_pixel_formats(&bytes)).unwrap_or_default(),
            })
        })
    }

    /// Idle advertised format: the registered default (an explicit choice),
    /// else this user's last published format, else NV12 640x480@30.
    pub(crate) fn idle_format(&self) -> VideoFormat {
        self.default_format
            .or_else(|| decode_format(reg_binary(Hive::CurrentUser, &format!("{HISTORY_KEY}\\{}", self.id), values::LAST_FORMAT, WireVideoFormat::SIZE)))
            .unwrap_or(FALLBACK_FORMAT)
    }

    /// Formats offered around `base` (the current format): `base` first,
    /// then every other registered pixel format at its size and rate.
    pub(crate) fn offered(&self, base: VideoFormat) -> Vec<VideoFormat> {
        let others = self
            .pixel_formats
            .iter()
            .filter(|format| **format != base.pixel_format)
            .map(|format| VideoFormat::new(*format, base.width, base.height, base.frame_rate));
        core::iter::once(base).chain(others).filter(|format| format.validate().is_ok()).collect()
    }
}
