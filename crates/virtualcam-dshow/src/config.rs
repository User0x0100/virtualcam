//! Camera configuration read from the manager's registry records.

use virtualcam_protocol::registry::{BACKEND_DIRECTSHOW, CAMERAS_KEY, HISTORY_KEY, MAX_FRAME_RATES, MAX_PIXEL_FORMATS, decode_frame_rates, decode_pixel_formats, values};
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
    /// Registered frame rates offered beside the current one; empty for
    /// cameras registered before multi-rate support.
    pub(crate) frame_rates: Vec<FrameRate>,
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
                frame_rates: reg_binary(hive, &key, values::FRAME_RATES, MAX_FRAME_RATES * 8).map(|bytes| decode_frame_rates(&bytes)).unwrap_or_default(),
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

    /// Formats offered around `base` (the current format), all at its size:
    /// each pixel format (`base`'s first, then the registered ones) at each
    /// frame rate (`base`'s first, then the registered ones), so `base` leads.
    pub(crate) fn offered(&self, base: VideoFormat) -> Vec<VideoFormat> {
        let interval = base.frame_rate.interval_100ns();
        let rates: Vec<FrameRate> = core::iter::once(base.frame_rate).chain(self.frame_rates.iter().copied().filter(|rate| rate.interval_100ns() != interval)).collect();
        let pixels = core::iter::once(base.pixel_format).chain(self.pixel_formats.iter().copied().filter(|format| *format != base.pixel_format));
        pixels
            .flat_map(|pixel| {
                rates.iter().map(move |rate| {
                    if pixel == base.pixel_format {
                        // Keep the current format's colorimetry at every rate.
                        VideoFormat { frame_rate: *rate, ..base }
                    } else {
                        VideoFormat::new(pixel, base.width, base.height, *rate)
                    }
                })
            })
            .filter(|format| format.validate().is_ok())
            .collect()
    }
}
