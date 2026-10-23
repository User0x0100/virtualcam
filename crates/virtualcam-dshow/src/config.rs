//! Camera configuration read from the manager's registry records.

use virtualcam_protocol::registry::{BACKEND_DIRECTSHOW, CAMERAS_KEY, HISTORY_KEY, values};
use virtualcam_protocol::{CameraId, FrameRate, PixelFormat, VideoFormat, WireVideoFormat};

use crate::win::{Hive, reg_binary, reg_string, reg_u32};

/// Format advertised when neither a producer, history, nor a default exists.
pub(crate) const FALLBACK_FORMAT: VideoFormat = VideoFormat::new(PixelFormat::Nv12, 640, 480, FrameRate::fps(30));

#[derive(Clone, Debug)]
pub(crate) struct CameraConfig {
    pub(crate) id: CameraId,
    pub(crate) name: String,
    pub(crate) default_format: Option<VideoFormat>,
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
            })
        })
    }

    /// Idle advertised format: this user's last published format, else the
    /// registered default, else NV12 640x480@30.
    pub(crate) fn idle_format(&self) -> VideoFormat {
        decode_format(reg_binary(Hive::CurrentUser, &format!("{HISTORY_KEY}\\{}", self.id), values::LAST_FORMAT, WireVideoFormat::SIZE))
            .or(self.default_format)
            .unwrap_or(FALLBACK_FORMAT)
    }
}
