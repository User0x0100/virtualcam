use crate::PixelFormat;
#[cfg(windows)]
use crate::{Error, Result};

/// Selects the installed virtual-camera protocol used by a camera instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraType {
    /// OBS Studio's Windows virtual-camera shared queue.
    Obs,
    /// This crate's independently registered per-session NV12 DirectShow camera.
    ManagedNv12,
    /// The Unity Capture `SharedImageMemory` protocol.
    UnityCapture,
}

/// OBS's rational frame-rate representation.
///
/// The upstream producer computes its 100-nanosecond interval as
/// `denominator * 10_000_000 / numerator`, using integer truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObsFrameRate {
    /// Frames in one rate period; must be non-zero.
    pub numerator: u32,
    /// Seconds represented by one rate period; must be non-zero.
    pub denominator: u32,
}

impl Default for ObsFrameRate {
    fn default() -> Self {
        Self { numerator: 30, denominator: 1 }
    }
}

/// Complete producer controls for OBS Virtual Camera.
///
/// OBS's upstream shared queue only controls source dimensions and frame
/// interval. Its input format is always NV12. `force_open` is this crate's
/// explicit recovery extension and is isolated to the OBS backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObsOptions {
    /// NV12 source width in pixels; must be non-zero and even.
    pub width: u32,
    /// NV12 source height in pixels; must be non-zero and even.
    pub height: u32,
    /// Exact rational rate used to calculate the queue interval.
    pub frame_rate: ObsFrameRate,
    /// Bypasses OBS's conservative existing-mapping occupancy check.
    ///
    /// This does not verify whether another writer is active, terminate a
    /// process, or close another process's handles. The caller guarantees that
    /// no other producer is writing. Multiple writers race on shared protocol
    /// fields and make output undefined.
    pub force_open: bool,
}

impl Default for ObsOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            frame_rate: ObsFrameRate::default(),
            force_open: false,
        }
    }
}

/// Pixel encodings accepted by the pinned Unity Capture receiver.
#[repr(i32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnityCaptureFormat {
    /// Four unsigned 8-bit RGBA components per pixel.
    #[default]
    Rgba8 = 0,
    /// Four IEEE-754 binary16 RGBA components interpreted as gamma-space data.
    Rgba16FloatGamma = 1,
    /// Four IEEE-754 binary16 RGBA components interpreted as linear-space data.
    Rgba16FloatLinear = 2,
}

impl UnityCaptureFormat {
    /// Returns the corresponding input format reported by [`crate::VirtualCamera`].
    pub const fn pixel_format(self) -> PixelFormat {
        match self {
            Self::Rgba8 => PixelFormat::Rgba8,
            Self::Rgba16FloatGamma => PixelFormat::Rgba16FloatGamma,
            Self::Rgba16FloatLinear => PixelFormat::Rgba16FloatLinear,
        }
    }

    /// Returns the source bytes occupied by one RGBA pixel.
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
            Self::Rgba16FloatGamma | Self::Rgba16FloatLinear => 8,
        }
    }
}

/// Unity Capture's receiver-side resize behavior.
#[repr(i32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnityCaptureResizeMode {
    /// Require producer and consumer dimensions to match; upstream's default.
    #[default]
    Disabled = 0,
    /// Ask the receiver to linearly scale the producer image to its output size.
    Linear = 1,
}

/// Unity Capture's receiver-side mirror behavior.
#[repr(i32)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnityCaptureMirrorMode {
    /// Preserve the source orientation; upstream's default.
    #[default]
    Disabled = 0,
    /// Mirror the converted output horizontally in the receiver.
    Horizontal = 1,
}

/// Complete shared-memory producer controls for Unity Capture.
///
/// The D3D11 sender's texture readback and double-buffering controls are not
/// shared-memory protocol controls and therefore do not apply to host/CUDA
/// buffers supplied directly to this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnityCaptureOptions {
    /// Source image width in pixels.
    pub width: u32,
    /// Source image height in pixels.
    pub height: u32,
    /// Zero-based Unity Capture device/capture number; zero is device #1.
    pub capture_number: u8,
    /// Source pixel encoding written into the shared header.
    pub format: UnityCaptureFormat,
    /// Source row stride in pixels. `None` selects tightly packed `width`.
    ///
    /// Upstream expresses stride in pixels for all three supported formats.
    pub stride_pixels: Option<u32>,
    /// Receiver-side resolution mismatch behavior.
    pub resize_mode: UnityCaptureResizeMode,
    /// Receiver-side horizontal mirror behavior.
    pub mirror_mode: UnityCaptureMirrorMode,
    /// Milliseconds without a new frame before the receiver reports stopped input.
    ///
    /// This is an upstream C `int` and is passed through unchanged.
    pub timeout_ms: i32,
}

impl UnityCaptureOptions {
    /// Largest capture number representable by the upstream object-name rule.
    pub const MAX_CAPTURE_NUMBER: u8 = b'z' - b'0';
    /// Largest frame allocation accepted by the upstream receiver mapping.
    pub const MAX_FRAME_BYTES: usize = 3840 * 2160 * 4 * core::mem::size_of::<i16>();
}

impl Default for UnityCaptureOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            capture_number: 0,
            format: UnityCaptureFormat::Rgba8,
            stride_pixels: None,
            resize_mode: UnityCaptureResizeMode::Disabled,
            mirror_mode: UnityCaptureMirrorMode::Disabled,
            timeout_ms: 1000,
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedObsOptions {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) frame_size: usize,
    pub(crate) interval_100ns: u64,
    pub(crate) force_open: bool,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct ValidatedUnityCaptureOptions {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) capture_number: u8,
    pub(crate) format: UnityCaptureFormat,
    pub(crate) stride_pixels: u32,
    pub(crate) frame_size: usize,
    pub(crate) resize_mode: UnityCaptureResizeMode,
    pub(crate) mirror_mode: UnityCaptureMirrorMode,
    pub(crate) timeout_ms: i32,
}

#[cfg(windows)]
impl ObsOptions {
    pub(crate) fn validate(self) -> Result<ValidatedObsOptions> {
        if self.width == 0 || self.height == 0 || !self.width.is_multiple_of(2) || !self.height.is_multiple_of(2) {
            return Err(Error::InvalidDimensions);
        }
        if self.frame_rate.numerator == 0 || self.frame_rate.denominator == 0 {
            return Err(Error::InvalidFrameRate);
        }

        let frame_size = PixelFormat::Nv12.frame_size(self.width, self.height)?;
        let interval = u128::from(self.frame_rate.denominator) * 10_000_000 / u128::from(self.frame_rate.numerator);
        let interval_100ns = u64::try_from(interval).map_err(|_| Error::InvalidFrameRate)?;
        if interval_100ns == 0 {
            return Err(Error::InvalidFrameRate);
        }

        Ok(ValidatedObsOptions {
            width: self.width,
            height: self.height,
            frame_size,
            interval_100ns,
            force_open: self.force_open,
        })
    }
}

#[cfg(windows)]
impl UnityCaptureOptions {
    pub(crate) fn validate(self) -> Result<ValidatedUnityCaptureOptions> {
        if self.width == 0 || self.height == 0 || self.width > i32::MAX as u32 || self.height > i32::MAX as u32 {
            return Err(Error::InvalidDimensions);
        }
        if self.capture_number > Self::MAX_CAPTURE_NUMBER {
            return Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "capture number exceeds Unity Capture protocol maximum",
            });
        }

        let stride_pixels = self.stride_pixels.unwrap_or(self.width);
        if stride_pixels < self.width || stride_pixels > i32::MAX as u32 {
            return Err(Error::InvalidDimensions);
        }
        let frame_size = usize::try_from(stride_pixels)
            .ok()
            .and_then(|stride| usize::try_from(self.height).ok().and_then(|height| stride.checked_mul(height)))
            .and_then(|pixels| pixels.checked_mul(self.format.bytes_per_pixel()))
            .ok_or(Error::FrameSizeOverflow)?;
        if frame_size > Self::MAX_FRAME_BYTES || u32::try_from(frame_size).is_err() {
            return Err(Error::InvalidDimensions);
        }

        Ok(ValidatedUnityCaptureOptions {
            width: self.width,
            height: self.height,
            capture_number: self.capture_number,
            format: self.format,
            stride_pixels,
            frame_size,
            resize_mode: self.resize_mode,
            mirror_mode: self.mirror_mode,
            timeout_ms: self.timeout_ms,
        })
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream_controls() {
        assert_eq!(
            ObsOptions::default(),
            ObsOptions {
                width: 1920,
                height: 1080,
                frame_rate: ObsFrameRate { numerator: 30, denominator: 1 },
                force_open: false,
            }
        );
        assert_eq!(
            UnityCaptureOptions::default(),
            UnityCaptureOptions {
                width: 1920,
                height: 1080,
                capture_number: 0,
                format: UnityCaptureFormat::Rgba8,
                stride_pixels: None,
                resize_mode: UnityCaptureResizeMode::Disabled,
                mirror_mode: UnityCaptureMirrorMode::Disabled,
                timeout_ms: 1000,
            }
        );
    }

    #[test]
    fn obs_uses_exact_rational_interval_and_validates_rate() {
        let options = ObsOptions {
            width: 2,
            height: 2,
            frame_rate: ObsFrameRate { numerator: 30_000, denominator: 1001 },
            force_open: false,
        };
        assert_eq!(options.validate().expect("valid rate").interval_100ns, 333_666);

        for frame_rate in [
            ObsFrameRate { numerator: 0, denominator: 1 },
            ObsFrameRate { numerator: 1, denominator: 0 },
            ObsFrameRate { numerator: u32::MAX, denominator: 1 },
        ] {
            assert!(matches!(ObsOptions { frame_rate, ..options }.validate(), Err(Error::InvalidFrameRate)));
        }
    }

    #[test]
    fn obs_rejects_zero_and_odd_nv12_dimensions() {
        for (width, height) in [(0, 2), (2, 0), (3, 2), (2, 3)] {
            assert!(matches!(ObsOptions { width, height, ..ObsOptions::default() }.validate(), Err(Error::InvalidDimensions)));
        }
    }

    #[test]
    fn unity_formats_and_stride_produce_upstream_data_sizes() {
        for (format, bytes_per_pixel) in [(UnityCaptureFormat::Rgba8, 4), (UnityCaptureFormat::Rgba16FloatGamma, 8), (UnityCaptureFormat::Rgba16FloatLinear, 8)] {
            let validated = UnityCaptureOptions {
                width: 3,
                height: 2,
                format,
                stride_pixels: Some(4),
                ..UnityCaptureOptions::default()
            }
            .validate()
            .expect("valid Unity options");
            assert_eq!(validated.frame_size, 4 * 2 * bytes_per_pixel);
            assert_eq!(validated.stride_pixels, 4);
        }
    }

    #[test]
    fn unity_rejects_invalid_dimensions_stride_capture_and_size() {
        for options in [
            UnityCaptureOptions {
                width: 0,
                ..UnityCaptureOptions::default()
            },
            UnityCaptureOptions {
                width: i32::MAX as u32 + 1,
                ..UnityCaptureOptions::default()
            },
            UnityCaptureOptions {
                width: 10,
                stride_pixels: Some(9),
                ..UnityCaptureOptions::default()
            },
            UnityCaptureOptions {
                width: 3841,
                height: 2160,
                format: UnityCaptureFormat::Rgba16FloatLinear,
                ..UnityCaptureOptions::default()
            },
        ] {
            assert!(matches!(options.validate(), Err(Error::InvalidDimensions)));
        }

        assert!(matches!(
            UnityCaptureOptions {
                capture_number: UnityCaptureOptions::MAX_CAPTURE_NUMBER + 1,
                ..UnityCaptureOptions::default()
            }
            .validate(),
            Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "capture number exceeds Unity Capture protocol maximum",
            })
        ));
    }
}
