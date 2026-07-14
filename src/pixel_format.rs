use crate::{Error, Result};

/// Native pixel formats accepted by the supported backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Tightly packed NV12: a full-resolution Y plane followed by an interleaved UV plane.
    Nv12,
    /// Tightly packed four-byte RGBA pixels, matching Unity Capture `FORMAT_UINT8`.
    Rgba8,
    /// Four binary16 RGBA components interpreted as gamma-space data by Unity Capture.
    Rgba16FloatGamma,
    /// Four binary16 RGBA components interpreted as linear-space data by Unity Capture.
    Rgba16FloatLinear,
}

impl PixelFormat {
    /// Computes the exact tightly packed byte length for `width` by `height`.
    ///
    /// Returns [`Error::InvalidDimensions`] when NV12 dimensions are odd or any
    /// dimension is zero, and [`Error::FrameSizeOverflow`] for arithmetic overflow.
    pub fn frame_size(self, width: u32, height: u32) -> Result<usize> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidDimensions);
        }
        if self == Self::Nv12 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
            return Err(Error::InvalidDimensions);
        }
        let width = usize::try_from(width).map_err(|_| Error::FrameSizeOverflow)?;
        let height = usize::try_from(height).map_err(|_| Error::FrameSizeOverflow)?;
        let pixels = width.checked_mul(height).ok_or(Error::FrameSizeOverflow)?;
        match self {
            Self::Nv12 => pixels.checked_mul(3).map(|bytes| bytes / 2).ok_or(Error::FrameSizeOverflow),
            Self::Rgba8 => pixels.checked_mul(4).ok_or(Error::FrameSizeOverflow),
            Self::Rgba16FloatGamma | Self::Rgba16FloatLinear => pixels.checked_mul(8).ok_or(Error::FrameSizeOverflow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_tightly_packed_sizes() {
        assert_eq!(PixelFormat::Nv12.frame_size(1920, 1080), Ok(3_110_400));
        assert_eq!(PixelFormat::Rgba8.frame_size(1920, 1080), Ok(8_294_400));
        assert_eq!(PixelFormat::Rgba16FloatGamma.frame_size(1920, 1080), Ok(16_588_800));
        assert_eq!(PixelFormat::Rgba16FloatLinear.frame_size(1920, 1080), Ok(16_588_800));
    }

    #[test]
    fn rejects_zero_and_odd_nv12_dimensions() {
        for (width, height) in [(0, 2), (2, 0), (3, 2), (2, 3)] {
            assert_eq!(PixelFormat::Nv12.frame_size(width, height), Err(Error::InvalidDimensions));
        }
        assert_eq!(PixelFormat::Rgba16FloatLinear.frame_size(0, 2), Err(Error::InvalidDimensions));
    }

    #[test]
    fn detects_rgba_size_overflow() {
        assert_eq!(PixelFormat::Rgba8.frame_size(u32::MAX, u32::MAX), Err(Error::FrameSizeOverflow));
    }
}
