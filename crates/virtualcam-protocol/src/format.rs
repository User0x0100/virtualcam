//! Pixel formats, colorimetry, frame rates, and their wire encoding.
//!
//! Producers always supply *tightly packed* frames in the layout described by
//! [`PixelFormat::layout`]. Sources copy them plane-by-plane into whatever
//! pitch the consumer's buffer uses; no source ever converts pixels.

use core::fmt;
use core::mem::{align_of, offset_of, size_of};

/// Largest accepted width or height of a managed camera frame.
pub const MAX_DIMENSION: u32 = 16_384;
/// Shortest accepted frame interval (1000 fps), in 100 ns units.
pub const MIN_INTERVAL_100NS: u64 = 10_000;
/// Longest accepted frame interval (one frame per minute), in 100 ns units.
pub const MAX_INTERVAL_100NS: u64 = 600_000_000;

/// Errors produced while validating or decoding a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// The pixel format cannot be carried by managed cameras.
    UnsupportedPixelFormat,
    /// Width/height are zero, too large, or not multiples of the chroma block.
    InvalidDimensions,
    /// The frame-rate numerator/denominator is zero or outside the accepted range.
    InvalidFrameRate,
    /// A computed byte size overflowed.
    FrameSizeOverflow,
    /// A wire record had an unknown code or a non-zero reserved field.
    InvalidWireFormat,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedPixelFormat => "pixel format is not supported by managed cameras",
            Self::InvalidDimensions => "invalid frame dimensions for the pixel format",
            Self::InvalidFrameRate => "invalid frame rate",
            Self::FrameSizeOverflow => "frame size overflow",
            Self::InvalidWireFormat => "invalid wire video format",
        })
    }
}

/// Pixel encodings understood by the crate.
///
/// Managed cameras (DirectShow and Media Foundation) carry every variant for
/// which [`Self::is_managed`] is true. The three RGBA variants exist only for
/// the third-party Unity Capture protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PixelFormat {
    /// 8-bit 4:2:0: full-resolution Y plane, then one interleaved UV plane.
    Nv12,
    /// 8-bit 4:2:0 planar: Y plane, then U plane, then V plane (also "IYUV").
    I420,
    /// 8-bit 4:2:2 packed `Y0 U Y1 V`.
    Yuy2,
    /// 8-bit 4:2:2 packed `U Y0 V Y1`.
    Uyvy,
    /// 10-bit 4:2:0 in 16-bit little-endian words (data in the high 10 bits):
    /// Y plane, then one interleaved UV plane.
    P010,
    /// 32-bit `B G R A` (Windows `RGB32`/`ARGB32`), top-down rows.
    Bgra32,
    /// 24-bit `B G R` (Windows `RGB24`), top-down rows, no row padding.
    Rgb24,
    /// Four 8-bit RGBA components; Unity Capture only.
    Rgba8,
    /// Four binary16 RGBA components in gamma space; Unity Capture only.
    Rgba16FloatGamma,
    /// Four binary16 RGBA components in linear space; Unity Capture only.
    Rgba16FloatLinear,
}

impl PixelFormat {
    /// Every format a managed camera can carry, in preference order.
    pub const MANAGED: [Self; 7] = [Self::Nv12, Self::I420, Self::Yuy2, Self::Uyvy, Self::P010, Self::Bgra32, Self::Rgb24];

    /// Stable transport code, or `None` for formats managed cameras cannot carry.
    pub const fn wire_code(self) -> Option<u32> {
        Some(match self {
            Self::Nv12 => 1,
            Self::I420 => 2,
            Self::Yuy2 => 3,
            Self::Uyvy => 4,
            Self::P010 => 5,
            Self::Bgra32 => 6,
            Self::Rgb24 => 7,
            Self::Rgba8 | Self::Rgba16FloatGamma | Self::Rgba16FloatLinear => return None,
        })
    }

    /// Inverse of [`Self::wire_code`].
    pub const fn from_wire_code(code: u32) -> Option<Self> {
        Some(match code {
            1 => Self::Nv12,
            2 => Self::I420,
            3 => Self::Yuy2,
            4 => Self::Uyvy,
            5 => Self::P010,
            6 => Self::Bgra32,
            7 => Self::Rgb24,
            _ => return None,
        })
    }

    /// Reports whether managed DirectShow/Media Foundation cameras carry this format.
    pub const fn is_managed(self) -> bool {
        self.wire_code().is_some()
    }

    /// Reports whether this is a Y'CbCr format (as opposed to RGB).
    pub const fn is_yuv(self) -> bool {
        matches!(self, Self::Nv12 | Self::I420 | Self::Yuy2 | Self::Uyvy | Self::P010)
    }

    /// The FourCC used in DirectShow/Media Foundation subtype GUIDs, or `None`
    /// for RGB formats that use dedicated subtype GUIDs.
    pub const fn fourcc(self) -> Option<u32> {
        Some(u32::from_le_bytes(match self {
            Self::Nv12 => *b"NV12",
            Self::I420 => *b"I420",
            Self::Yuy2 => *b"YUY2",
            Self::Uyvy => *b"UYVY",
            Self::P010 => *b"P010",
            _ => return None,
        }))
    }

    /// Average bits per pixel (`BITMAPINFOHEADER::biBitCount`).
    pub const fn bits_per_pixel(self) -> u16 {
        match self {
            Self::Nv12 | Self::I420 => 12,
            Self::Yuy2 | Self::Uyvy => 16,
            Self::P010 | Self::Rgb24 => 24,
            Self::Bgra32 | Self::Rgba8 => 32,
            Self::Rgba16FloatGamma | Self::Rgba16FloatLinear => 64,
        }
    }

    /// Required width and height multiples imposed by chroma subsampling.
    pub const fn block_size(self) -> (u32, u32) {
        match self {
            Self::Nv12 | Self::I420 | Self::P010 => (2, 2),
            Self::Yuy2 | Self::Uyvy => (2, 1),
            _ => (1, 1),
        }
    }

    /// Bytes per pixel of the first plane.
    const fn luma_bytes(self) -> usize {
        match self {
            Self::Nv12 | Self::I420 => 1,
            Self::Yuy2 | Self::Uyvy | Self::P010 => 2,
            Self::Rgb24 => 3,
            Self::Bgra32 | Self::Rgba8 => 4,
            Self::Rgba16FloatGamma | Self::Rgba16FloatLinear => 8,
        }
    }

    /// Computes the tightly packed layout producers must supply.
    pub fn layout(self, width: u32, height: u32) -> Result<FrameLayout, FormatError> {
        let (bw, bh) = self.block_size();
        if width == 0 || height == 0 || !width.is_multiple_of(bw) || !height.is_multiple_of(bh) {
            return Err(FormatError::InvalidDimensions);
        }
        let row = (width as usize).checked_mul(self.luma_bytes()).ok_or(FormatError::FrameSizeOverflow)?;
        self.layout_with_pitch(width, height, row)
    }

    /// Computes the layout of a buffer whose first plane uses `pitch` bytes
    /// per row, following the Windows convention for planar formats: chroma
    /// planes follow the luma plane contiguously; NV12/P010 chroma rows use
    /// `pitch` and I420 chroma rows use `pitch / 2`.
    pub fn layout_with_pitch(self, width: u32, height: u32, pitch: usize) -> Result<FrameLayout, FormatError> {
        let (bw, bh) = self.block_size();
        if width == 0 || height == 0 || !width.is_multiple_of(bw) || !height.is_multiple_of(bh) {
            return Err(FormatError::InvalidDimensions);
        }
        let w = width as usize;
        let h = height as usize;
        let row = w.checked_mul(self.luma_bytes()).ok_or(FormatError::FrameSizeOverflow)?;
        if pitch < row {
            return Err(FormatError::InvalidDimensions);
        }
        let luma = pitch.checked_mul(h).ok_or(FormatError::FrameSizeOverflow)?;
        let first = Plane { offset: 0, pitch, row_bytes: row, rows: h };
        let (planes, count, size) = match self {
            Self::Nv12 | Self::P010 => {
                let chroma = Plane {
                    offset: luma,
                    pitch,
                    row_bytes: row,
                    rows: h / 2,
                };
                let size = luma.checked_add(pitch.checked_mul(h / 2).ok_or(FormatError::FrameSizeOverflow)?).ok_or(FormatError::FrameSizeOverflow)?;
                ([first, chroma, Plane::EMPTY], 2, size)
            }
            Self::I420 => {
                // Planar chroma pitch is half the luma pitch, so an odd pitch
                // cannot describe a valid I420 buffer.
                if !pitch.is_multiple_of(2) {
                    return Err(FormatError::InvalidDimensions);
                }
                let chroma_pitch = pitch / 2;
                let chroma_size = chroma_pitch.checked_mul(h / 2).ok_or(FormatError::FrameSizeOverflow)?;
                let u = Plane {
                    offset: luma,
                    pitch: chroma_pitch,
                    row_bytes: w / 2,
                    rows: h / 2,
                };
                let v = Plane {
                    offset: luma + chroma_size,
                    pitch: chroma_pitch,
                    row_bytes: w / 2,
                    rows: h / 2,
                };
                let size = luma.checked_add(chroma_size * 2).ok_or(FormatError::FrameSizeOverflow)?;
                ([first, u, v], 3, size)
            }
            _ => ([first, Plane::EMPTY, Plane::EMPTY], 1, luma),
        };
        Ok(FrameLayout { planes, plane_count: count, size })
    }

    /// Computes the tightly packed frame size in bytes.
    pub fn frame_size(self, width: u32, height: u32) -> Result<usize, FormatError> {
        self.layout(width, height).map(|layout| layout.size)
    }
}

/// One plane inside a frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// Byte offset of the first row from the start of the buffer.
    pub offset: usize,
    /// Bytes between the starts of consecutive rows.
    pub pitch: usize,
    /// Visible bytes in one row (`<= pitch`).
    pub row_bytes: usize,
    /// Number of rows.
    pub rows: usize,
}

impl Plane {
    const EMPTY: Self = Self {
        offset: 0,
        pitch: 0,
        row_bytes: 0,
        rows: 0,
    };
}

/// Plane geometry of one frame buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    planes: [Plane; 3],
    plane_count: usize,
    size: usize,
}

impl FrameLayout {
    /// Planes in memory order.
    pub fn planes(&self) -> &[Plane] {
        &self.planes[..self.plane_count]
    }

    /// Total bytes spanned by all planes.
    pub const fn size(&self) -> usize {
        self.size
    }
}

/// Rational frame rate `numerator / denominator` frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRate {
    /// Frames in one rate period; must be non-zero.
    pub numerator: u32,
    /// Seconds represented by one rate period; must be non-zero.
    pub denominator: u32,
}

impl FrameRate {
    /// Builds an integral frames-per-second rate.
    pub const fn fps(fps: u32) -> Self {
        Self { numerator: fps, denominator: 1 }
    }

    /// Frame interval in 100 ns units, `denominator * 10^7 / numerator`
    /// truncated (identical to OBS's integer computation), or `None` when
    /// either term is zero.
    pub const fn interval_100ns(self) -> Option<u64> {
        if self.numerator == 0 || self.denominator == 0 {
            return None;
        }
        Some(self.denominator as u64 * 10_000_000 / self.numerator as u64)
    }
}

impl Default for FrameRate {
    fn default() -> Self {
        Self::fps(30)
    }
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $code:expr),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            /// Not signaled; consumers apply their own default.
            #[default]
            Unspecified,
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Windows `MFVideo*` / DXVA numeric value of this variant.
            pub const fn code(self) -> u8 {
                match self {
                    Self::Unspecified => 0,
                    $(Self::$variant => $code,)+
                }
            }

            /// Inverse of [`Self::code`].
            pub const fn from_code(code: u8) -> Option<Self> {
                match code {
                    0 => Some(Self::Unspecified),
                    $($code => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

wire_enum!(
    /// Y'CbCr -> R'G'B' matrix (`MFVideoTransferMatrix`).
    ColorMatrix {
        /// ITU-R BT.709.
        Bt709 = 1,
        /// ITU-R BT.601.
        Bt601 = 2,
        /// SMPTE 240M.
        Smpte240m = 3,
        /// ITU-R BT.2020 non-constant luminance.
        Bt2020 = 4,
    }
);

wire_enum!(
    /// Nominal code range (`MFNominalRange`).
    ColorRange {
        /// Full range, 0..=255 for 8-bit.
        Full = 1,
        /// Limited ("studio") range, 16..=235 luma for 8-bit.
        Limited = 2,
    }
);

wire_enum!(
    /// Color primaries (`MFVideoPrimaries`).
    ColorPrimaries {
        /// ITU-R BT.709 / sRGB.
        Bt709 = 2,
        /// ITU-R BT.470-2 System M.
        Bt470M = 3,
        /// ITU-R BT.470-2 System B,G (625-line BT.601).
        Bt470Bg = 4,
        /// SMPTE 170M (525-line BT.601).
        Smpte170m = 5,
        /// SMPTE 240M.
        Smpte240m = 6,
        /// ITU-R BT.2020.
        Bt2020 = 9,
        /// DCI-P3.
        DciP3 = 11,
    }
);

wire_enum!(
    /// Opto-electronic transfer function (`MFVideoTransferFunction`).
    TransferFunction {
        /// Linear light.
        Linear = 1,
        /// Pure gamma 2.2.
        Gamma22 = 4,
        /// ITU-R BT.709 (also used by BT.601 and SDR BT.2020).
        Bt709 = 5,
        /// SMPTE 240M.
        Smpte240m = 6,
        /// sRGB.
        Srgb = 7,
        /// Pure gamma 2.8.
        Gamma28 = 8,
        /// SMPTE ST 2084 perceptual quantizer (HDR10).
        Pq = 15,
        /// ARIB STD-B67 hybrid log-gamma.
        Hlg = 16,
    }
);

/// Colorimetry signaled to consumers alongside the pixel format.
///
/// It is metadata only: sources never convert pixels. Each field maps 1:1 to
/// the Media Foundation `MF_MT_*` attribute and the DirectShow
/// `DXVA_ExtendedFormat` bits of `VIDEOINFOHEADER2::dwControlFlags`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ColorSpace {
    /// Y'CbCr matrix; ignored for RGB formats.
    pub matrix: ColorMatrix,
    /// Nominal code range.
    pub range: ColorRange,
    /// Color primaries.
    pub primaries: ColorPrimaries,
    /// Transfer function.
    pub transfer: TransferFunction,
}

impl ColorSpace {
    /// SD video: BT.601 matrix, limited range, SMPTE 170M primaries.
    pub const BT601: Self = Self::new(ColorMatrix::Bt601, ColorRange::Limited, ColorPrimaries::Smpte170m, TransferFunction::Bt709);
    /// HD video: BT.709, limited range.
    pub const BT709: Self = Self::new(ColorMatrix::Bt709, ColorRange::Limited, ColorPrimaries::Bt709, TransferFunction::Bt709);
    /// BT.709 with full-range code values (common for screen/webcam content).
    pub const BT709_FULL: Self = Self::new(ColorMatrix::Bt709, ColorRange::Full, ColorPrimaries::Bt709, TransferFunction::Bt709);
    /// SDR wide-gamut UHD: BT.2020, limited range.
    pub const BT2020: Self = Self::new(ColorMatrix::Bt2020, ColorRange::Limited, ColorPrimaries::Bt2020, TransferFunction::Bt709);
    /// HDR10: BT.2020 with the PQ transfer function, limited range.
    pub const BT2100_PQ: Self = Self::new(ColorMatrix::Bt2020, ColorRange::Limited, ColorPrimaries::Bt2020, TransferFunction::Pq);
    /// HLG HDR: BT.2020 with hybrid log-gamma, limited range.
    pub const BT2100_HLG: Self = Self::new(ColorMatrix::Bt2020, ColorRange::Limited, ColorPrimaries::Bt2020, TransferFunction::Hlg);
    /// sRGB for RGB formats.
    pub const SRGB: Self = Self::new(ColorMatrix::Unspecified, ColorRange::Full, ColorPrimaries::Bt709, TransferFunction::Srgb);

    /// Builds a color space from its four components.
    pub const fn new(matrix: ColorMatrix, range: ColorRange, primaries: ColorPrimaries, transfer: TransferFunction) -> Self {
        Self { matrix, range, primaries, transfer }
    }

    /// Conventional default for `format` at `height`: sRGB for RGB, BT.2020
    /// for 10-bit, BT.709 for HD 8-bit, BT.601 for SD 8-bit.
    pub const fn default_for(format: PixelFormat, height: u32) -> Self {
        match format {
            PixelFormat::Bgra32 | PixelFormat::Rgb24 | PixelFormat::Rgba8 | PixelFormat::Rgba16FloatGamma | PixelFormat::Rgba16FloatLinear => Self::SRGB,
            PixelFormat::P010 => Self::BT2020,
            _ if height >= 720 => Self::BT709,
            _ => Self::BT601,
        }
    }

    /// Reports whether black is encoded with full-range code values.
    pub const fn is_full_range(self, format: PixelFormat) -> bool {
        match self.range {
            ColorRange::Full => true,
            ColorRange::Limited => false,
            // Windows treats unsignaled RGB as full range and YUV as limited.
            _ => !format.is_yuv(),
        }
    }
}

/// A complete stream description: pixel format, size, rate, and colorimetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoFormat {
    /// Pixel encoding of every frame.
    pub pixel_format: PixelFormat,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Nominal frame rate advertised to consumers.
    pub frame_rate: FrameRate,
    /// Colorimetry signaled to consumers.
    pub color: ColorSpace,
}

impl VideoFormat {
    /// Builds a format with the conventional [`ColorSpace::default_for`] colorimetry.
    pub const fn new(pixel_format: PixelFormat, width: u32, height: u32, frame_rate: FrameRate) -> Self {
        Self {
            pixel_format,
            width,
            height,
            frame_rate,
            color: ColorSpace::default_for(pixel_format, height),
        }
    }

    /// Returns this format with explicit colorimetry.
    pub const fn with_color(mut self, color: ColorSpace) -> Self {
        self.color = color;
        self
    }

    /// Validates the format for managed cameras and precomputes its layout.
    pub fn validate(self) -> Result<ValidatedFormat, FormatError> {
        if !self.pixel_format.is_managed() {
            return Err(FormatError::UnsupportedPixelFormat);
        }
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION {
            return Err(FormatError::InvalidDimensions);
        }
        let layout = self.pixel_format.layout(self.width, self.height)?;
        // DirectShow carries the image size in a DWORD.
        if u32::try_from(layout.size()).is_err() {
            return Err(FormatError::FrameSizeOverflow);
        }
        let interval_100ns = self.frame_rate.interval_100ns().ok_or(FormatError::InvalidFrameRate)?;
        if !(MIN_INTERVAL_100NS..=MAX_INTERVAL_100NS).contains(&interval_100ns) {
            return Err(FormatError::InvalidFrameRate);
        }
        Ok(ValidatedFormat { format: self, layout, interval_100ns })
    }

    /// Encodes the format into its fixed wire record.
    pub fn to_wire(self) -> Result<WireVideoFormat, FormatError> {
        Ok(WireVideoFormat {
            pixel_format: self.pixel_format.wire_code().ok_or(FormatError::UnsupportedPixelFormat)?,
            width: self.width,
            height: self.height,
            fps_numerator: self.frame_rate.numerator,
            fps_denominator: self.frame_rate.denominator,
            matrix: self.color.matrix.code(),
            range: self.color.range.code(),
            primaries: self.color.primaries.code(),
            transfer: self.color.transfer.code(),
            reserved: [0; 2],
        })
    }
}

impl fmt::Display for VideoFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {}x{} @ {}/{}", self.pixel_format, self.width, self.height, self.frame_rate.numerator, self.frame_rate.denominator)
    }
}

/// A [`VideoFormat`] proven valid for managed cameras, with derived values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedFormat {
    format: VideoFormat,
    layout: FrameLayout,
    interval_100ns: u64,
}

impl ValidatedFormat {
    /// The validated format.
    pub const fn format(&self) -> VideoFormat {
        self.format
    }

    /// Tightly packed producer layout.
    pub const fn layout(&self) -> &FrameLayout {
        &self.layout
    }

    /// Tightly packed frame size in bytes.
    pub const fn frame_size(&self) -> usize {
        self.layout.size
    }

    /// Frame interval in 100 ns units.
    pub const fn interval_100ns(&self) -> u64 {
        self.interval_100ns
    }
}

/// Fixed 32-byte little-endian wire record of a [`VideoFormat`].
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WireVideoFormat {
    /// [`PixelFormat::wire_code`].
    pub pixel_format: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frame-rate numerator.
    pub fps_numerator: u32,
    /// Frame-rate denominator.
    pub fps_denominator: u32,
    /// [`ColorMatrix::code`].
    pub matrix: u8,
    /// [`ColorRange::code`].
    pub range: u8,
    /// [`ColorPrimaries::code`].
    pub primaries: u8,
    /// [`TransferFunction::code`].
    pub transfer: u8,
    /// Must be zero.
    pub reserved: [u32; 2],
}

impl WireVideoFormat {
    /// Byte length of the encoded record.
    pub const SIZE: usize = 32;

    /// Decodes the record without range-validating dimensions; call
    /// [`VideoFormat::validate`] before trusting it for allocation.
    pub fn decode(self) -> Result<VideoFormat, FormatError> {
        if self.reserved != [0; 2] {
            return Err(FormatError::InvalidWireFormat);
        }
        let invalid = FormatError::InvalidWireFormat;
        Ok(VideoFormat {
            pixel_format: PixelFormat::from_wire_code(self.pixel_format).ok_or(invalid)?,
            width: self.width,
            height: self.height,
            frame_rate: FrameRate {
                numerator: self.fps_numerator,
                denominator: self.fps_denominator,
            },
            color: ColorSpace {
                matrix: ColorMatrix::from_code(self.matrix).ok_or(invalid)?,
                range: ColorRange::from_code(self.range).ok_or(invalid)?,
                primaries: ColorPrimaries::from_code(self.primaries).ok_or(invalid)?,
                transfer: TransferFunction::from_code(self.transfer).ok_or(invalid)?,
            },
        })
    }

    /// Serializes to little-endian bytes (registry/handshake storage).
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0_u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.pixel_format.to_le_bytes());
        out[4..8].copy_from_slice(&self.width.to_le_bytes());
        out[8..12].copy_from_slice(&self.height.to_le_bytes());
        out[12..16].copy_from_slice(&self.fps_numerator.to_le_bytes());
        out[16..20].copy_from_slice(&self.fps_denominator.to_le_bytes());
        out[20] = self.matrix;
        out[21] = self.range;
        out[22] = self.primaries;
        out[23] = self.transfer;
        out[24..28].copy_from_slice(&self.reserved[0].to_le_bytes());
        out[28..32].copy_from_slice(&self.reserved[1].to_le_bytes());
        out
    }

    /// Parses little-endian bytes produced by [`Self::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FormatError> {
        let bytes: &[u8; Self::SIZE] = bytes.try_into().map_err(|_| FormatError::InvalidWireFormat)?;
        let u32_at = |offset: usize| u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]);
        Ok(Self {
            pixel_format: u32_at(0),
            width: u32_at(4),
            height: u32_at(8),
            fps_numerator: u32_at(12),
            fps_denominator: u32_at(16),
            matrix: bytes[20],
            range: bytes[21],
            primaries: bytes[22],
            transfer: bytes[23],
            reserved: [u32_at(24), u32_at(28)],
        })
    }
}

const _: () = {
    assert!(size_of::<WireVideoFormat>() == WireVideoFormat::SIZE);
    assert!(align_of::<WireVideoFormat>() == 4);
    assert!(offset_of!(WireVideoFormat, fps_denominator) == 16);
    assert!(offset_of!(WireVideoFormat, matrix) == 20);
    assert!(offset_of!(WireVideoFormat, reserved) == 24);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightly_packed_sizes_match_windows_conventions() {
        let cases = [
            (PixelFormat::Nv12, 1920 * 1080 * 3 / 2),
            (PixelFormat::I420, 1920 * 1080 * 3 / 2),
            (PixelFormat::Yuy2, 1920 * 1080 * 2),
            (PixelFormat::Uyvy, 1920 * 1080 * 2),
            (PixelFormat::P010, 1920 * 1080 * 3),
            (PixelFormat::Bgra32, 1920 * 1080 * 4),
            (PixelFormat::Rgb24, 1920 * 1080 * 3),
            (PixelFormat::Rgba16FloatLinear, 1920 * 1080 * 8),
        ];
        for (format, size) in cases {
            assert_eq!(format.frame_size(1920, 1080), Ok(size), "{format:?}");
        }
    }

    #[test]
    fn planar_layouts_follow_windows_plane_order() {
        let i420 = PixelFormat::I420.layout(4, 2).expect("valid I420");
        assert_eq!(i420.planes().len(), 3);
        assert_eq!(
            i420.planes()[1],
            Plane {
                offset: 8,
                pitch: 2,
                row_bytes: 2,
                rows: 1
            }
        );
        assert_eq!(
            i420.planes()[2],
            Plane {
                offset: 10,
                pitch: 2,
                row_bytes: 2,
                rows: 1
            }
        );

        let nv12 = PixelFormat::Nv12.layout_with_pitch(4, 2, 64).expect("pitched NV12");
        assert_eq!(
            nv12.planes()[1],
            Plane {
                offset: 128,
                pitch: 64,
                row_bytes: 4,
                rows: 1
            }
        );
        assert_eq!(nv12.size(), 192);
        assert_eq!(PixelFormat::Nv12.layout_with_pitch(4, 2, 3), Err(FormatError::InvalidDimensions));
        assert_eq!(PixelFormat::I420.layout_with_pitch(4, 2, 5), Err(FormatError::InvalidDimensions));
    }

    #[test]
    fn chroma_block_constraints_are_enforced() {
        assert_eq!(PixelFormat::Nv12.layout(3, 2), Err(FormatError::InvalidDimensions));
        assert_eq!(PixelFormat::Nv12.layout(2, 3), Err(FormatError::InvalidDimensions));
        assert!(PixelFormat::Yuy2.layout(2, 3).is_ok());
        assert_eq!(PixelFormat::Yuy2.layout(3, 2), Err(FormatError::InvalidDimensions));
        assert!(PixelFormat::Rgb24.layout(3, 3).is_ok());
        assert_eq!(PixelFormat::Rgb24.layout(0, 3), Err(FormatError::InvalidDimensions));
    }

    #[test]
    fn validation_rejects_unity_formats_rates_and_sizes() {
        let ok = VideoFormat::new(PixelFormat::Nv12, 1920, 1080, FrameRate::fps(60));
        let validated = ok.validate().expect("valid");
        assert_eq!(validated.interval_100ns(), 166_666);
        assert_eq!(validated.frame_size(), 3_110_400);
        assert_eq!(VideoFormat { pixel_format: PixelFormat::Rgba8, ..ok }.validate(), Err(FormatError::UnsupportedPixelFormat));
        assert_eq!(VideoFormat { width: MAX_DIMENSION + 2, ..ok }.validate(), Err(FormatError::InvalidDimensions));
        for frame_rate in [FrameRate::fps(0), FrameRate { numerator: 1, denominator: 0 }, FrameRate::fps(1001), FrameRate { numerator: 1, denominator: 61 }] {
            assert_eq!(VideoFormat { frame_rate, ..ok }.validate(), Err(FormatError::InvalidFrameRate), "{frame_rate:?}");
        }
        assert_eq!(FrameRate { numerator: 30_000, denominator: 1001 }.interval_100ns(), Some(333_666));
    }

    #[test]
    fn default_colorimetry_follows_format_and_height() {
        assert_eq!(VideoFormat::new(PixelFormat::Nv12, 640, 480, FrameRate::default()).color, ColorSpace::BT601);
        assert_eq!(VideoFormat::new(PixelFormat::Nv12, 1280, 720, FrameRate::default()).color, ColorSpace::BT709);
        assert_eq!(VideoFormat::new(PixelFormat::Bgra32, 1280, 720, FrameRate::default()).color, ColorSpace::SRGB);
        assert_eq!(VideoFormat::new(PixelFormat::P010, 3840, 2160, FrameRate::default()).color, ColorSpace::BT2020);
        assert!(ColorSpace::default().is_full_range(PixelFormat::Bgra32));
        assert!(!ColorSpace::default().is_full_range(PixelFormat::Nv12));
        assert!(ColorSpace::BT709_FULL.is_full_range(PixelFormat::Nv12));
    }

    #[test]
    fn wire_record_round_trips_and_rejects_garbage() {
        let format = VideoFormat::new(PixelFormat::P010, 3840, 2160, FrameRate { numerator: 60_000, denominator: 1001 }).with_color(ColorSpace::BT2100_PQ);
        let wire = format.to_wire().expect("encodable");
        let bytes = wire.to_bytes();
        assert_eq!(&bytes[0..4], &5_u32.to_le_bytes());
        assert_eq!(WireVideoFormat::from_bytes(&bytes).expect("parse").decode(), Ok(format));

        assert_eq!(WireVideoFormat::from_bytes(&bytes[..31]), Err(FormatError::InvalidWireFormat));
        for (offset, value) in [(0, 99_u8), (20, 200), (21, 9), (24, 1)] {
            let mut corrupt = bytes;
            corrupt[offset] = value;
            assert_eq!(WireVideoFormat::from_bytes(&corrupt).expect("parse").decode(), Err(FormatError::InvalidWireFormat), "offset {offset}");
        }
    }

    #[test]
    fn color_codes_match_media_foundation_enums() {
        assert_eq!(ColorMatrix::Bt709.code(), 1);
        assert_eq!(ColorMatrix::Bt601.code(), 2);
        assert_eq!(ColorRange::Full.code(), 1);
        assert_eq!(ColorRange::Limited.code(), 2);
        assert_eq!(ColorPrimaries::Bt2020.code(), 9);
        assert_eq!(TransferFunction::Srgb.code(), 7);
        assert_eq!(TransferFunction::Pq.code(), 15);
        assert_eq!(TransferFunction::Hlg.code(), 16);
    }
}
