//! `AM_MEDIA_TYPE` construction, parsing, and ownership.

use core::mem::{ManuallyDrop, size_of};

use virtualcam_protocol::dshow::guids;
use virtualcam_protocol::{FrameLayout, FrameRate, PixelFormat, VideoFormat};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::DirectShow::{AMCONTROL_COLORINFO_PRESENT, AMCONTROL_USED};
use windows::Win32::Media::MediaFoundation::{AM_MEDIA_TYPE, VIDEOINFOHEADER, VIDEOINFOHEADER2, VIDEOINFOHEADER2_0};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows_core::GUID;

/// Format block flavor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Header {
    /// `FORMAT_VideoInfo` — the most widely accepted.
    VideoInfo,
    /// `FORMAT_VideoInfo2` — additionally carries colorimetry.
    VideoInfo2,
}

/// Every header flavor, in enumeration preference order.
pub(crate) const HEADERS: [Header; 2] = [Header::VideoInfo, Header::VideoInfo2];

/// Bytes per DIB row: DWORD-aligned for RGB, tight for YUV FourCCs.
pub(crate) fn dib_pitch(format: PixelFormat, width: u32) -> usize {
    let bits = width as usize * format.bits_per_pixel() as usize;
    if format.is_yuv() {
        // Planar formats report their average bpp; the first-plane pitch is
        // the luma width in bytes.
        match format {
            PixelFormat::Nv12 | PixelFormat::I420 => width as usize,
            PixelFormat::P010 => width as usize * 2,
            _ => bits / 8,
        }
    } else {
        bits.div_ceil(32) * 4
    }
}

/// Layout of one downstream sample buffer.
pub(crate) fn sample_layout(format: &VideoFormat) -> Option<FrameLayout> {
    format.pixel_format.layout_with_pitch(format.width, format.height, dib_pitch(format.pixel_format, format.width)).ok()
}

/// A media type whose format block is owned (`CoTaskMemAlloc`).
pub(crate) struct MediaType(pub(crate) AM_MEDIA_TYPE);

// SAFETY: The format block is plain heap memory; `pUnk` is always null for
// the types this crate builds and is released on drop for copies.
unsafe impl Send for MediaType {}
// SAFETY: Shared access only reads the immutable format block.
unsafe impl Sync for MediaType {}

impl MediaType {
    /// Builds the media type describing `format`.
    pub(crate) fn new(format: &VideoFormat, header: Header) -> Option<Self> {
        let layout = sample_layout(format)?;
        let size = u32::try_from(layout.size()).ok()?;
        let interval = format.frame_rate.interval_100ns()? as i64;
        let rect = RECT {
            left: 0,
            top: 0,
            right: format.width as i32,
            bottom: format.height as i32,
        };
        let bitrate = (u64::from(size) * 8 * 10_000_000 / interval.max(1) as u64).min(u64::from(u32::MAX)) as u32;
        let bmi = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: format.width as i32,
            biHeight: format.height as i32,
            biPlanes: 1,
            biBitCount: format.pixel_format.bits_per_pixel(),
            biCompression: format.pixel_format.fourcc().unwrap_or(0),
            biSizeImage: size,
            ..BITMAPINFOHEADER::default()
        };
        let (formattype, block) = match header {
            Header::VideoInfo => (
                guids::FORMAT_VIDEO_INFO,
                bytes_of(&VIDEOINFOHEADER {
                    rcSource: rect,
                    rcTarget: rect,
                    dwBitRate: bitrate,
                    dwBitErrorRate: 0,
                    AvgTimePerFrame: interval,
                    bmiHeader: bmi,
                }),
            ),
            Header::VideoInfo2 => {
                let divisor = gcd(format.width, format.height).max(1);
                (
                    guids::FORMAT_VIDEO_INFO2,
                    bytes_of(&VIDEOINFOHEADER2 {
                        rcSource: rect,
                        rcTarget: rect,
                        dwBitRate: bitrate,
                        dwBitErrorRate: 0,
                        AvgTimePerFrame: interval,
                        dwInterlaceFlags: 0,
                        dwCopyProtectFlags: 0,
                        dwPictAspectRatioX: format.width / divisor,
                        dwPictAspectRatioY: format.height / divisor,
                        Anonymous: VIDEOINFOHEADER2_0 { dwControlFlags: control_flags(format) },
                        dwReserved2: 0,
                        bmiHeader: bmi,
                    }),
                )
            }
        };
        Some(Self::from_parts(GUID::from_u128(format.pixel_format.dshow_subtype()?), size, GUID::from_u128(formattype), &block))
    }

    fn from_parts(subtype: GUID, sample_size: u32, formattype: GUID, block: &[u8]) -> Self {
        // SAFETY: Allocation of the format block, freed in `Drop`.
        let pb = unsafe { CoTaskMemAlloc(block.len()) }.cast::<u8>();
        let cb = if pb.is_null() {
            0
        } else {
            // SAFETY: `pb` has `block.len()` writable bytes.
            unsafe { core::ptr::copy_nonoverlapping(block.as_ptr(), pb, block.len()) };
            block.len() as u32
        };
        Self(AM_MEDIA_TYPE {
            majortype: GUID::from_u128(guids::MEDIATYPE_VIDEO),
            subtype,
            bFixedSizeSamples: true.into(),
            bTemporalCompression: false.into(),
            lSampleSize: sample_size,
            formattype,
            pUnk: ManuallyDrop::new(None),
            cbFormat: cb,
            pbFormat: pb,
        })
    }

    /// Deep-copies `self` into caller-provided storage (`ConnectionMediaType`).
    ///
    /// # Safety
    ///
    /// `target` must be writable; its previous contents are overwritten
    /// without being freed, as the DirectShow contract requires.
    pub(crate) unsafe fn copy_to(&self, target: *mut AM_MEDIA_TYPE) {
        let block = self.format_block();
        let copy = Self::from_parts(self.0.subtype, self.0.lSampleSize, self.0.formattype, block);
        let copy = ManuallyDrop::new(copy);
        // SAFETY: Ownership of the format block moves into `target`.
        unsafe { core::ptr::write(target, core::ptr::read(&copy.0)) };
    }

    /// Returns a `CoTaskMemAlloc`'d deep copy (`DeleteMediaType` ownership).
    pub(crate) fn to_heap(&self) -> *mut AM_MEDIA_TYPE {
        // SAFETY: Allocation of one AM_MEDIA_TYPE released by the caller.
        let target = unsafe { CoTaskMemAlloc(size_of::<AM_MEDIA_TYPE>()) }.cast::<AM_MEDIA_TYPE>();
        if !target.is_null() {
            // SAFETY: `target` is writable storage for one AM_MEDIA_TYPE.
            unsafe { self.copy_to(target) };
        }
        target
    }

    fn format_block(&self) -> &[u8] {
        if self.0.pbFormat.is_null() {
            &[]
        } else {
            // SAFETY: `pbFormat` owns `cbFormat` bytes.
            unsafe { core::slice::from_raw_parts(self.0.pbFormat, self.0.cbFormat as usize) }
        }
    }
}

impl Clone for MediaType {
    fn clone(&self) -> Self {
        Self::from_parts(self.0.subtype, self.0.lSampleSize, self.0.formattype, self.format_block())
    }
}

impl Drop for MediaType {
    fn drop(&mut self) {
        if !self.0.pbFormat.is_null() {
            // SAFETY: The block was allocated with CoTaskMemAlloc.
            unsafe { CoTaskMemFree(Some(self.0.pbFormat.cast())) };
            self.0.pbFormat = core::ptr::null_mut();
        }
        // SAFETY: Dropped exactly once.
        unsafe { ManuallyDrop::drop(&mut self.0.pUnk) };
    }
}

fn bytes_of<T>(value: &T) -> Vec<u8> {
    // SAFETY: `T` is a plain `repr(C)` Win32 structure without padding-
    // sensitive invariants; its bytes are copied verbatim.
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
}

const fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// `VIDEOINFOHEADER2::dwControlFlags`: AMCONTROL flags in bits 0..8 and the
/// `DXVA_ExtendedFormat` fields above them. Our color codes are the
/// MF/DXVA numeric values.
fn control_flags(format: &VideoFormat) -> u32 {
    let color = format.color;
    let mut flags = AMCONTROL_USED | AMCONTROL_COLORINFO_PRESENT;
    flags |= u32::from(color.range.code() & 0x7) << 12;
    flags |= u32::from(color.matrix.code() & 0x7) << 15;
    flags |= u32::from(color.primaries.code() & 0x1F) << 22;
    flags |= u32::from(color.transfer.code() & 0x1F) << 27;
    flags
}

/// A consumer-supplied media type decoded into our terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub(crate) pixel_format: PixelFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// 0 when unspecified.
    pub(crate) interval_100ns: i64,
    /// RGB rows are top-down (`biHeight < 0`).
    pub(crate) top_down: bool,
}

impl Parsed {
    /// Reports whether this type describes `wanted` (pixel format and size
    /// exactly; frame interval unspecified or within 1 unit of rounding).
    pub(crate) fn matches(&self, wanted: &VideoFormat) -> bool {
        let interval = wanted.frame_rate.interval_100ns().map_or(0, |v| v as i64);
        self.pixel_format == wanted.pixel_format && self.width == wanted.width && self.height == wanted.height && (self.interval_100ns == 0 || (self.interval_100ns - interval).abs() <= 1)
    }

    /// The format this type selects, keeping `base`'s colorimetry and using
    /// its frame rate when the type leaves the interval unspecified.
    pub(crate) fn to_format(self, base: &VideoFormat) -> VideoFormat {
        let base_interval = base.frame_rate.interval_100ns().map_or(0, |v| v as i64);
        let frame_rate = if self.interval_100ns == 0 || (self.interval_100ns - base_interval).abs() <= 1 {
            base.frame_rate
        } else if self.interval_100ns <= i64::from(u32::MAX) {
            let interval = self.interval_100ns as u32;
            let divisor = gcd(10_000_000, interval).max(1);
            FrameRate {
                numerator: 10_000_000 / divisor,
                denominator: interval / divisor,
            }
        } else {
            base.frame_rate
        };
        let mut format = VideoFormat::new(self.pixel_format, self.width, self.height, frame_rate);
        if base.pixel_format == self.pixel_format {
            format.color = base.color;
        }
        format
    }
}

/// Decodes a video media type; `None` for anything this source cannot produce.
///
/// # Safety
///
/// `mt` must be null or point to a valid `AM_MEDIA_TYPE` whose `pbFormat`
/// holds `cbFormat` readable bytes.
pub(crate) unsafe fn parse(mt: *const AM_MEDIA_TYPE) -> Option<Parsed> {
    // SAFETY: Caller contract.
    let mt = unsafe { mt.as_ref() }?;
    if mt.majortype != GUID::from_u128(guids::MEDIATYPE_VIDEO) || mt.pbFormat.is_null() {
        return None;
    }
    let pixel_format = PixelFormat::from_dshow_subtype(mt.subtype.to_u128())?;
    let (interval, bmi) = if mt.formattype == GUID::from_u128(guids::FORMAT_VIDEO_INFO) && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER>() {
        // SAFETY: Size checked; unaligned read of the caller's block.
        let vih = unsafe { mt.pbFormat.cast::<VIDEOINFOHEADER>().read_unaligned() };
        (vih.AvgTimePerFrame, vih.bmiHeader)
    } else if mt.formattype == GUID::from_u128(guids::FORMAT_VIDEO_INFO2) && mt.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>() {
        // SAFETY: Size checked; unaligned read of the caller's block.
        let vih = unsafe { mt.pbFormat.cast::<VIDEOINFOHEADER2>().read_unaligned() };
        (vih.AvgTimePerFrame, vih.bmiHeader)
    } else {
        return None;
    };
    if bmi.biWidth <= 0 || bmi.biHeight == 0 || bmi.biHeight == i32::MIN || interval < 0 {
        return None;
    }
    let width = bmi.biWidth as u32;
    let height = bmi.biHeight.unsigned_abs();
    VideoFormat::new(pixel_format, width, height, FrameRate::default()).validate().ok()?;
    Some(Parsed {
        pixel_format,
        width,
        height,
        interval_100ns: interval,
        top_down: bmi.biHeight < 0 && !pixel_format.is_yuv(),
    })
}

#[cfg(test)]
mod tests {
    use virtualcam_protocol::ColorSpace;

    use super::*;

    #[test]
    fn built_types_parse_back_for_every_managed_format_and_header() {
        for pixel_format in PixelFormat::MANAGED {
            let format = VideoFormat::new(pixel_format, 1280, 720, FrameRate::fps(60));
            for header in HEADERS {
                let mt = MediaType::new(&format, header).expect("build");
                // SAFETY: `mt` is a valid owned media type.
                let parsed = unsafe { parse(&mt.0) }.expect("parse");
                assert!(parsed.matches(&format), "{pixel_format:?} {header:?}");
                assert_eq!(parsed.to_format(&format), format);
                assert_eq!(mt.0.lSampleSize as usize, sample_layout(&format).expect("layout").size());
            }
        }
    }

    #[test]
    fn rgb_rows_are_dword_aligned_and_yuv_rows_tight() {
        assert_eq!(dib_pitch(PixelFormat::Rgb24, 3), 12);
        assert_eq!(dib_pitch(PixelFormat::Bgra32, 3), 12);
        assert_eq!(dib_pitch(PixelFormat::Nv12, 6), 6);
        assert_eq!(dib_pitch(PixelFormat::Yuy2, 6), 12);
        assert_eq!(dib_pitch(PixelFormat::P010, 6), 12);
        let format = VideoFormat::new(PixelFormat::Rgb24, 3, 2, FrameRate::fps(30));
        assert_eq!(MediaType::new(&format, Header::VideoInfo).expect("build").0.lSampleSize, 24);
    }

    #[test]
    fn colorimetry_lands_in_dxva_extended_format_bits() {
        let format = VideoFormat::new(PixelFormat::Nv12, 1920, 1080, FrameRate::fps(30)).with_color(ColorSpace::BT709);
        let flags = control_flags(&format);
        assert_eq!(flags & 0xFF, AMCONTROL_USED | AMCONTROL_COLORINFO_PRESENT);
        assert_eq!((flags >> 12) & 0x7, 2, "limited range = DXVA_NominalRange_16_235");
        assert_eq!((flags >> 15) & 0x7, 1, "BT.709 matrix");
        assert_eq!((flags >> 22) & 0x1F, 2, "BT.709 primaries");
        assert_eq!((flags >> 27) & 0x1F, 5, "BT.709 transfer");
    }

    #[test]
    fn parse_rejects_foreign_and_truncated_types() {
        let format = VideoFormat::new(PixelFormat::Nv12, 64, 64, FrameRate::fps(30));
        let mut mt = MediaType::new(&format, Header::VideoInfo).expect("build");
        mt.0.cbFormat = 10;
        // SAFETY: Valid type with a shortened declared block.
        assert_eq!(unsafe { parse(&mt.0) }, None);
        let mut mt = MediaType::new(&format, Header::VideoInfo).expect("build");
        mt.0.majortype = GUID::zeroed();
        // SAFETY: Valid type.
        assert_eq!(unsafe { parse(&mt.0) }, None);
        // SAFETY: Null is allowed.
        assert_eq!(unsafe { parse(core::ptr::null()) }, None);
    }

    #[test]
    fn rgb_top_down_and_frame_rate_reduction() {
        let format = VideoFormat::new(PixelFormat::Bgra32, 4, 4, FrameRate { numerator: 30_000, denominator: 1001 });
        let mt = MediaType::new(&format, Header::VideoInfo).expect("build");
        // SAFETY: Exclusive owned block of VIDEOINFOHEADER size.
        unsafe { (*mt.0.pbFormat.cast::<VIDEOINFOHEADER>()).bmiHeader.biHeight = -4 };
        // SAFETY: Valid type.
        let parsed = unsafe { parse(&mt.0) }.expect("parse");
        assert!(parsed.top_down);
        assert_eq!(parsed.to_format(&format).frame_rate.interval_100ns(), Some(333_666));
    }
}
