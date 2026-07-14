#[cfg(windows)]
use crate::{Error, PixelFormat, Result};

/// Selects the installed virtual-camera protocol used by a camera instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraType {
    /// OBS Studio's Windows virtual-camera shared queue.
    Obs,
    /// The Unity Capture `SharedImageMemory` protocol, capture number zero.
    UnityCapture,
}

impl CameraType {
    #[cfg(windows)]
    pub(crate) const fn native_format(self) -> PixelFormat {
        match self {
            Self::Obs => PixelFormat::Nv12,
            Self::UnityCapture => PixelFormat::Rgba8,
        }
    }
}

/// Image dimensions and nominal frame rate for a virtual camera producer.
#[derive(Debug, Clone)]
pub struct CameraConfig {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Nominal frames per second; must be finite and greater than zero.
    pub fps: f64,
}

/// Options that modify how a producer is opened.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Bypasses OBS's conservative existing-mapping occupancy check.
    ///
    /// This is disabled by default and has no extra effect for Unity Capture.
    /// It does not verify whether another writer is active, terminate a process,
    /// or close another process's handles. The caller guarantees that no other
    /// producer is writing. Multiple writers race on shared protocol fields and
    /// make output undefined.
    pub force_open: bool,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedConfig {
    pub(crate) width: u32,
    pub(crate) height: u32,
    #[cfg(feature = "tracing")]
    pub(crate) fps: f64,
    pub(crate) frame_size: usize,
    pub(crate) obs_interval_100ns: u64,
}

#[cfg(windows)]
impl CameraConfig {
    pub(crate) fn validate(&self, camera_type: CameraType) -> Result<ValidatedConfig> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidDimensions);
        }
        if !self.fps.is_finite() || self.fps <= 0.0 {
            return Err(Error::InvalidFrameRate);
        }

        let native_format = camera_type.native_format();
        if native_format == PixelFormat::Nv12
            && (!self.width.is_multiple_of(2) || !self.height.is_multiple_of(2))
        {
            return Err(Error::InvalidDimensions);
        }
        if camera_type == CameraType::UnityCapture
            && (self.width > i32::MAX as u32 || self.height > i32::MAX as u32)
        {
            return Err(Error::InvalidDimensions);
        }

        let frame_size = native_format.checked_frame_size(self.width, self.height)?;
        let interval = 10_000_000.0 / self.fps;
        if !interval.is_finite() || interval < 1.0 || interval > u64::MAX as f64 {
            return Err(Error::InvalidFrameRate);
        }

        Ok(ValidatedConfig {
            width: self.width,
            height: self.height,
            #[cfg(feature = "tracing")]
            fps: self.fps,
            frame_size,
            obs_interval_100ns: interval as u64,
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn rejects_odd_nv12_dimensions() {
        let config = CameraConfig {
            width: 3,
            height: 2,
            fps: 30.0,
        };
        assert!(matches!(
            config.validate(CameraType::Obs),
            Err(Error::InvalidDimensions)
        ));
    }

    #[test]
    fn accepts_odd_rgba_dimensions() {
        let config = CameraConfig {
            width: 3,
            height: 5,
            fps: 60.0,
        };
        let validated = config
            .validate(CameraType::UnityCapture)
            .expect("valid RGBA config");
        assert_eq!(validated.frame_size, 60);
    }

    #[test]
    fn validates_frame_rate() {
        for fps in [0.0, -1.0, f64::NAN, f64::INFINITY, 10_000_001.0, 1.0e-20] {
            let config = CameraConfig {
                width: 2,
                height: 2,
                fps,
            };
            assert!(matches!(
                config.validate(CameraType::Obs),
                Err(Error::InvalidFrameRate)
            ));
        }
    }

    #[test]
    fn rejects_zero_and_unrepresentable_unity_dimensions() {
        for (width, height) in [(0, 2), (2, 0), (i32::MAX as u32 + 1, 2)] {
            let config = CameraConfig {
                width,
                height,
                fps: 30.0,
            };
            assert!(matches!(
                config.validate(CameraType::UnityCapture),
                Err(Error::InvalidDimensions)
            ));
        }
    }

    #[test]
    fn obs_interval_uses_protocol_truncation() {
        let config = CameraConfig {
            width: 2,
            height: 2,
            fps: 30.0,
        };
        let validated = config.validate(CameraType::Obs).expect("valid config");
        assert_eq!(validated.obs_interval_100ns, 333_333);
    }
}
