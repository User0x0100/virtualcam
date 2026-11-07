//! Pixel conversions a Media Foundation source performs itself.
//!
//! A Media Foundation camera registers one native mode, which the producer
//! writes. Consumers that want another layout otherwise go through Windows'
//! own conversion in Frame Server's DirectShow bridge, which shears frames
//! whose YUY2 row is not 64-byte aligned (for example 1080 pixels wide). The
//! source therefore also offers the conversions listed here and performs them
//! while filling each sample.
//!
//! Only 8-bit 4:2:0 to 4:2:2 is supported: NV12 or I420 to YUY2. Chroma is
//! upsampled vertically with the MPEG-2 4:2:0 siting (chroma between the two
//! luma rows it covers): each output row blends its own chroma row 3:1 with
//! the nearest neighbor row, clamped at the edges. Matrix and range carry
//! over unchanged.

use crate::format::{FrameLayout, PixelFormat, Plane};
use crate::planes::CopyError;

/// Pixel formats a source can offer besides `native`, in preference order.
pub const fn conversions_from(native: PixelFormat) -> &'static [PixelFormat] {
    match native {
        PixelFormat::Nv12 | PixelFormat::I420 => &[PixelFormat::Yuy2],
        _ => &[],
    }
}

/// Whether [`convert_frame`] supports `src` -> `dst`.
pub fn can_convert(src: PixelFormat, dst: PixelFormat) -> bool {
    conversions_from(src).contains(&dst)
}

/// Where a 4:2:0 source keeps its chroma.
enum Chroma {
    /// NV12: one plane of (U, V) pairs.
    Interleaved(Plane),
    /// I420: separate U and V planes.
    Planar(Plane, Plane),
}

impl Chroma {
    /// Reads chroma row `row` as interleaved (U, V) pairs into `pairs`.
    ///
    /// # Safety
    ///
    /// `row` must be a valid chroma row of the planes, which must lie inside
    /// a readable frame at `src`.
    unsafe fn read(&self, src: *const u8, row: usize, pairs: &mut [u8], scratch: &mut [u8]) {
        match self {
            // SAFETY: Caller contract.
            Self::Interleaved(uv) => unsafe { read_row(src, uv.offset + row * uv.pitch, pairs) },
            Self::Planar(u, v) => {
                for (plane, slot) in [(u, 0), (v, 1)] {
                    // SAFETY: Caller contract.
                    unsafe { read_row(src, plane.offset + row * plane.pitch, scratch) };
                    for (pair, value) in pairs.chunks_exact_mut(2).zip(scratch.iter()) {
                        pair[slot] = *value;
                    }
                }
            }
        }
    }
}

/// Reads `len` bytes at `offset` of a frame that may be concurrently
/// modified, into `row`.
///
/// # Safety
///
/// `src + offset .. src + offset + row.len()` must be readable.
unsafe fn read_row(src: *const u8, offset: usize, row: &mut [u8]) {
    // SAFETY: Caller contract; the bytes are only copied.
    unsafe { core::ptr::copy_nonoverlapping(src.add(offset), row.as_mut_ptr(), row.len()) };
}

/// Converts one tight-or-pitched `src` frame into `dst`.
///
/// Both layouts must describe the same width and height (the source layout
/// for `src_format`, the destination layout for `dst_format`), and
/// [`can_convert`] must accept the pair.
///
/// # Safety
///
/// `src` must be readable for `src_layout.size()` bytes. The bytes may be
/// concurrently modified (seqlock reads); each row is copied once before use.
pub unsafe fn convert_frame(src: *const u8, src_format: PixelFormat, src_layout: &FrameLayout, dst: &mut [u8], dst_format: PixelFormat, dst_layout: &FrameLayout) -> Result<(), CopyError> {
    if !can_convert(src_format, dst_format) {
        return Err(CopyError);
    }
    let (luma, out) = match (src_layout.planes(), dst_layout.planes()) {
        ([luma, ..], [out]) => (luma, out),
        _ => return Err(CopyError),
    };
    let (width, height) = (luma.row_bytes, luma.rows);
    let chroma_rows = height / 2;
    if width == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) || out.rows != height || out.row_bytes != width * 2 {
        return Err(CopyError);
    }
    if out
        .rows
        .checked_sub(1)
        .and_then(|last| last.checked_mul(out.pitch))
        .and_then(|start| start.checked_add(out.offset + out.row_bytes))
        .is_none_or(|end| end > dst.len())
    {
        return Err(CopyError);
    }
    let chroma = match (src_format, src_layout.planes()) {
        (PixelFormat::Nv12, [_, uv]) if uv.row_bytes == width && uv.rows == chroma_rows => Chroma::Interleaved(*uv),
        (PixelFormat::I420, [_, u, v]) if u.row_bytes == width / 2 && v.row_bytes == width / 2 && u.rows == chroma_rows && v.rows == chroma_rows => Chroma::Planar(*u, *v),
        _ => return Err(CopyError),
    };
    let mut scratch = vec![0_u8; width / 2];
    // SAFETY: Every chroma row index below is < `chroma_rows`, inside the
    // caller-guaranteed source span.
    let mut read_chroma = |row: usize, pairs: &mut [u8]| unsafe { chroma.read(src, row, pairs, &mut scratch) };
    let mut y = vec![0_u8; width];
    // Rolling window of chroma rows k - 1, k, k + 1 (clamped).
    let mut rows = [vec![0_u8; width], vec![0_u8; width], vec![0_u8; width]];
    let mut loaded = usize::MAX;
    for row in 0..height {
        let k = row / 2;
        if loaded != k {
            if loaded.checked_add(1) == Some(k) {
                rows.rotate_left(1);
                read_chroma((k + 1).min(chroma_rows - 1), &mut rows[2]);
            } else {
                read_chroma(k.saturating_sub(1), &mut rows[0]);
                read_chroma(k, &mut rows[1]);
                read_chroma((k + 1).min(chroma_rows - 1), &mut rows[2]);
            }
            loaded = k;
        }
        let neighbor = if row.is_multiple_of(2) { &rows[0] } else { &rows[2] };
        // SAFETY: In bounds of the caller-guaranteed source span.
        unsafe { read_row(src, luma.offset + row * luma.pitch, &mut y) };
        let start = out.offset + row * out.pitch;
        let line = &mut dst[start..start + out.row_bytes];
        for (((group, luma), own), near) in line.chunks_exact_mut(4).zip(y.chunks_exact(2)).zip(rows[1].chunks_exact(2)).zip(neighbor.chunks_exact(2)) {
            let blend = |own: u8, near: u8| ((3 * u16::from(own) + u16::from(near) + 2) >> 2) as u8;
            group.copy_from_slice(&[luma[0], blend(own[0], near[0]), luma[1], blend(own[1], near[1])]);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12(width: usize, height: usize, y: impl Fn(usize, usize) -> u8, uv: impl Fn(usize, usize) -> (u8, u8)) -> Vec<u8> {
        let mut frame = vec![0_u8; width * height * 3 / 2];
        for row in 0..height {
            for x in 0..width {
                frame[row * width + x] = y(x, row);
            }
        }
        for row in 0..height / 2 {
            for x in 0..width / 2 {
                let (u, v) = uv(x, row);
                frame[width * height + row * width + 2 * x] = u;
                frame[width * height + row * width + 2 * x + 1] = v;
            }
        }
        frame
    }

    fn to_i420(nv12: &[u8], width: usize, height: usize) -> Vec<u8> {
        let mut frame = nv12[..width * height].to_vec();
        let uv = &nv12[width * height..];
        frame.extend(uv.iter().step_by(2));
        frame.extend(uv.iter().skip(1).step_by(2));
        frame
    }

    fn convert(src: &[u8], format: PixelFormat, width: u32, height: u32, pitch: usize) -> Vec<u8> {
        let src_layout = format.layout(width, height).expect("layout");
        let dst_layout = PixelFormat::Yuy2.layout_with_pitch(width, height, pitch).expect("yuy2");
        let mut dst = vec![0xEE_u8; dst_layout.size()];
        // SAFETY: `src` spans the tight layout.
        unsafe { convert_frame(src.as_ptr(), format, &src_layout, &mut dst, PixelFormat::Yuy2, &dst_layout) }.expect("convert");
        dst
    }

    #[test]
    fn offers_yuy2_only_from_8_bit_4_2_0() {
        assert_eq!(conversions_from(PixelFormat::Nv12), &[PixelFormat::Yuy2]);
        assert_eq!(conversions_from(PixelFormat::I420), &[PixelFormat::Yuy2]);
        for native in [PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::P010, PixelFormat::Bgra32, PixelFormat::Rgb24] {
            assert!(conversions_from(native).is_empty(), "{native:?}");
        }
    }

    #[test]
    fn luma_is_kept_and_flat_chroma_is_replicated() {
        let (w, h) = (6, 4);
        let src = nv12(w, h, |x, row| (row * 10 + x) as u8, |_, _| (90, 200));
        for (format, frame) in [(PixelFormat::Nv12, src.clone()), (PixelFormat::I420, to_i420(&src, w, h))] {
            let dst = convert(&frame, format, w as u32, h as u32, w * 2);
            for row in 0..h {
                for x in 0..w {
                    assert_eq!(dst[row * w * 2 + x * 2], (row * 10 + x) as u8, "{format:?} luma {x},{row}");
                }
                for pair in 0..w / 2 {
                    assert_eq!(&dst[row * w * 2 + pair * 4 + 1..][..1], &[90]);
                    assert_eq!(&dst[row * w * 2 + pair * 4 + 3..][..1], &[200]);
                }
            }
        }
    }

    #[test]
    fn chroma_is_interpolated_with_mpeg2_siting() {
        // Chroma rows 0, 100, 200 (U) for a 2x6 frame.
        let src = nv12(2, 6, |_, _| 16, |_, row| ((row * 100) as u8, 128));
        let dst = convert(&src, PixelFormat::Nv12, 2, 6, 4);
        let u: Vec<u8> = (0..6).map(|row| dst[row * 4 + 1]).collect();
        // Row 0: 3/4*0 + 1/4*0 (clamped); row 1: 3/4*0 + 1/4*100; row 2: 3/4*100 + 1/4*0; ...
        assert_eq!(u, [0, 25, 75, 125, 175, 200]);
    }

    #[test]
    fn pitched_destination_keeps_padding() {
        let src = nv12(2, 2, |x, _| x as u8 + 1, |_, _| (128, 128));
        let dst = convert(&src, PixelFormat::Nv12, 2, 2, 8);
        assert_eq!(&dst[..8], &[1, 128, 2, 128, 0xEE, 0xEE, 0xEE, 0xEE]);
        assert_eq!(&dst[8..12], &[1, 128, 2, 128]);
    }

    #[test]
    fn rejects_unsupported_pairs_and_short_destinations() {
        let layout = PixelFormat::Nv12.layout(2, 2).expect("layout");
        let yuy2 = PixelFormat::Yuy2.layout(2, 2).expect("layout");
        let src = [0_u8; 6];
        let mut short = vec![0_u8; 7];
        // SAFETY: Rejected before any read.
        unsafe {
            assert_eq!(convert_frame(src.as_ptr(), PixelFormat::Nv12, &layout, &mut short, PixelFormat::Yuy2, &yuy2), Err(CopyError));
            assert_eq!(convert_frame(src.as_ptr(), PixelFormat::Nv12, &layout, &mut [0; 6], PixelFormat::Nv12, &layout), Err(CopyError));
            let other = PixelFormat::Yuy2.layout(2, 4).expect("layout");
            assert_eq!(convert_frame(src.as_ptr(), PixelFormat::Nv12, &layout, &mut [0; 16], PixelFormat::Yuy2, &other), Err(CopyError));
        }
    }
}
