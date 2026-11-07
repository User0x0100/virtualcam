//! Pitch-aware frame copy and black fill.
//!
//! Sources copy each tightly packed ring frame straight into the consumer's
//! buffer, honoring its pitch row by row. When both layouts are tight the
//! copy collapses into one `memcpy`. Nothing is staged; pixel conversions live
//! in [`crate::convert`].

use crate::format::{FrameLayout, PixelFormat};

/// The destination buffer cannot hold the requested layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyError;

fn layout_fits(layout: &FrameLayout, len: usize) -> bool {
    layout.planes().iter().all(|plane| {
        plane.row_bytes <= plane.pitch
            && plane.rows.checked_sub(1).is_none_or(|last| {
                last.checked_mul(plane.pitch)
                    .and_then(|start| start.checked_add(plane.offset))
                    .and_then(|start| start.checked_add(plane.row_bytes))
                    .is_some_and(|end| end <= len)
            })
    })
}

fn compatible(src: &FrameLayout, dst: &FrameLayout) -> bool {
    src.planes().len() == dst.planes().len() && src.planes().iter().zip(dst.planes()).all(|(s, d)| s.row_bytes == d.row_bytes && s.rows == d.rows)
}

/// Copies one frame from `src` (described by `src_layout`) into `dst`.
///
/// With `flip_vertical`, destination row `i` receives source row
/// `rows - 1 - i` (bottom-up DIBs, negative Media Foundation pitch); it is
/// only meaningful for single-plane formats and rejected otherwise.
///
/// # Safety
///
/// `src` must be readable for `src_layout.size()` bytes. The bytes may be
/// concurrently modified (seqlock reads); they are only copied.
pub unsafe fn copy_frame(src: *const u8, src_layout: &FrameLayout, dst: &mut [u8], dst_layout: &FrameLayout, flip_vertical: bool) -> Result<(), CopyError> {
    if !compatible(src_layout, dst_layout) || !layout_fits(dst_layout, dst.len()) || (flip_vertical && dst_layout.planes().len() != 1) {
        return Err(CopyError);
    }
    let dst = dst.as_mut_ptr();
    if !flip_vertical && src_layout == dst_layout {
        // SAFETY: Identical layouts; the destination was bounds-checked and
        // the caller guarantees the source span.
        unsafe { core::ptr::copy_nonoverlapping(src, dst, src_layout.size()) };
        return Ok(());
    }
    for (s, d) in src_layout.planes().iter().zip(dst_layout.planes()) {
        for row in 0..d.rows {
            let src_row = if flip_vertical { d.rows - 1 - row } else { row };
            // SAFETY: Both rows are in bounds: the destination by
            // `layout_fits`, the source by the caller's contract.
            unsafe {
                core::ptr::copy_nonoverlapping(src.add(s.offset + src_row * s.pitch), dst.add(d.offset + row * d.pitch), d.row_bytes);
            }
        }
    }
    Ok(())
}

/// Byte pattern of one black pixel group for each plane of `format`.
fn black_patterns(format: PixelFormat, full_range: bool) -> [&'static [u8]; 3] {
    let y8: &'static [u8] = if full_range { &[0] } else { &[16] };
    match format {
        PixelFormat::Nv12 => [y8, &[128], &[]],
        PixelFormat::I420 => [y8, &[128], &[128]],
        PixelFormat::Yuy2 => [if full_range { &[0, 128] } else { &[16, 128] }, &[], &[]],
        PixelFormat::Uyvy => [if full_range { &[128, 0] } else { &[128, 16] }, &[], &[]],
        // 10-bit code 64 (limited black) / 512 (neutral chroma) in the high bits.
        PixelFormat::P010 => [if full_range { &[0, 0] } else { &[0x00, 0x10] }, &[0x00, 0x80], &[]],
        PixelFormat::Bgra32 | PixelFormat::Rgba8 => [&[0, 0, 0, 255], &[], &[]],
        PixelFormat::Rgb24 => [&[0], &[], &[]],
        PixelFormat::Rgba16FloatGamma | PixelFormat::Rgba16FloatLinear => [&[0, 0, 0, 0, 0, 0, 0x00, 0x3C], &[], &[]],
    }
}

/// Fills every visible row of `dst` with black for `format`.
pub fn fill_black(dst: &mut [u8], layout: &FrameLayout, format: PixelFormat, full_range: bool) -> Result<(), CopyError> {
    if !layout_fits(layout, dst.len()) {
        return Err(CopyError);
    }
    let patterns = black_patterns(format, full_range);
    for (plane, pattern) in layout.planes().iter().zip(patterns) {
        if pattern.is_empty() || !plane.row_bytes.is_multiple_of(pattern.len()) {
            return Err(CopyError);
        }
        for row in 0..plane.rows {
            let start = plane.offset + row * plane.pitch;
            let line = &mut dst[start..start + plane.row_bytes];
            if let [value] = pattern {
                line.fill(*value);
            } else {
                for chunk in line.chunks_exact_mut(pattern.len()) {
                    chunk.copy_from_slice(pattern);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tight_layouts_copy_verbatim() {
        let layout = PixelFormat::Nv12.layout(4, 2).expect("layout");
        let src: Vec<u8> = (0..12).collect();
        let mut dst = vec![0_u8; 12];
        // SAFETY: `src` spans the layout.
        unsafe { copy_frame(src.as_ptr(), &layout, &mut dst, &layout, false) }.expect("copy");
        assert_eq!(dst, src);
    }

    #[test]
    fn pitched_destination_receives_rows_and_keeps_padding() {
        let src_layout = PixelFormat::Nv12.layout(4, 2).expect("layout");
        let dst_layout = PixelFormat::Nv12.layout_with_pitch(4, 2, 8).expect("pitched");
        let src: Vec<u8> = (1..=12).collect();
        let mut dst = vec![0xEE_u8; dst_layout.size()];
        // SAFETY: `src` spans the tight layout.
        unsafe { copy_frame(src.as_ptr(), &src_layout, &mut dst, &dst_layout, false) }.expect("copy");
        assert_eq!(&dst[0..4], &[1, 2, 3, 4]);
        assert_eq!(&dst[4..8], &[0xEE; 4]);
        assert_eq!(&dst[8..12], &[5, 6, 7, 8]);
        assert_eq!(&dst[16..20], &[9, 10, 11, 12]);
    }

    #[test]
    fn flip_reverses_rows_of_packed_formats_only() {
        let layout = PixelFormat::Rgb24.layout(1, 3).expect("layout");
        let src = [1, 1, 1, 2, 2, 2, 3, 3, 3];
        let dst_layout = PixelFormat::Rgb24.layout_with_pitch(1, 3, 4).expect("DIB pitch");
        let mut dst = vec![0_u8; 12];
        // SAFETY: `src` spans the layout.
        unsafe { copy_frame(src.as_ptr(), &layout, &mut dst, &dst_layout, true) }.expect("flip");
        assert_eq!(dst, [3, 3, 3, 0, 2, 2, 2, 0, 1, 1, 1, 0]);
        let nv12 = PixelFormat::Nv12.layout(2, 2).expect("layout");
        // SAFETY: Rejected before any read.
        assert_eq!(unsafe { copy_frame(src.as_ptr(), &nv12, &mut dst, &nv12, true) }, Err(CopyError));
    }

    #[test]
    fn copy_rejects_short_or_mismatched_destinations() {
        let layout = PixelFormat::Nv12.layout(4, 2).expect("layout");
        let other = PixelFormat::Nv12.layout(2, 2).expect("layout");
        let src = [0_u8; 12];
        let mut short = [0_u8; 11];
        // SAFETY: Rejected before any read.
        unsafe {
            assert_eq!(copy_frame(src.as_ptr(), &layout, &mut short, &layout, false), Err(CopyError));
            assert_eq!(copy_frame(src.as_ptr(), &layout, &mut [0; 12], &other, false), Err(CopyError));
        }
    }

    #[test]
    fn black_matches_each_format_and_range() {
        let check = |format: PixelFormat, full: bool, expected: &[u8]| {
            let layout = format.layout(2, 2).expect("layout");
            let mut buffer = vec![0xAA; layout.size()];
            fill_black(&mut buffer, &layout, format, full).expect("fill");
            assert_eq!(buffer, expected, "{format:?} full={full}");
        };
        check(PixelFormat::Nv12, false, &[16, 16, 16, 16, 128, 128]);
        check(PixelFormat::Nv12, true, &[0, 0, 0, 0, 128, 128]);
        check(PixelFormat::I420, false, &[16, 16, 16, 16, 128, 128]);
        check(PixelFormat::Yuy2, false, &[16, 128, 16, 128, 16, 128, 16, 128]);
        check(PixelFormat::Uyvy, false, &[128, 16, 128, 16, 128, 16, 128, 16]);
        check(PixelFormat::P010, false, &[0, 0x10, 0, 0x10, 0, 0x10, 0, 0x10, 0, 0x80, 0, 0x80]);
        check(PixelFormat::Bgra32, true, &[0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
        check(PixelFormat::Rgb24, true, &[0; 12]);
    }

    #[test]
    fn black_respects_pitch_padding() {
        let layout = PixelFormat::Nv12.layout_with_pitch(2, 2, 4).expect("pitched");
        let mut buffer = vec![0xAA; layout.size()];
        fill_black(&mut buffer, &layout, PixelFormat::Nv12, false).expect("fill");
        assert_eq!(buffer, [16, 16, 0xAA, 0xAA, 16, 16, 0xAA, 0xAA, 128, 128, 0xAA, 0xAA]);
    }
}
