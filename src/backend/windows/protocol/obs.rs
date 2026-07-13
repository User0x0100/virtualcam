use core::mem::{align_of, offset_of, size_of};

use crate::{Error, Result};

pub(crate) const VIDEO_NAME: &[u16] = &[
    b'O' as u16,
    b'B' as u16,
    b'S' as u16,
    b'V' as u16,
    b'i' as u16,
    b'r' as u16,
    b't' as u16,
    b'u' as u16,
    b'a' as u16,
    b'l' as u16,
    b'C' as u16,
    b'a' as u16,
    b'm' as u16,
    b'V' as u16,
    b'i' as u16,
    b'd' as u16,
    b'e' as u16,
    b'o' as u16,
    0,
];

pub(crate) const STATE_STARTING: u32 = 1;
pub(crate) const STATE_READY: u32 = 2;
pub(crate) const STATE_STOPPING: u32 = 3;
pub(crate) const FRAME_HEADER_SIZE: usize = 32;
pub(crate) const SLOT_COUNT: usize = 3;

/// Mirrors `struct queue_header` from obsproject/obs-studio@ca937366c1e9fc7f22fd5f1c7d3a3a5f7951170b,
/// `shared/obs-shared-memory-queue/shared-memory-queue.c`.
///
/// The three leading `uint32_t` fields are volatile in C. Rust keeps their ABI
/// as plain integers and confines volatile access to the OBS backend.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct QueueHeader {
    pub(crate) write_idx: u32,
    pub(crate) read_idx: u32,
    pub(crate) state: u32,
    pub(crate) offsets: [u32; SLOT_COUNT],
    pub(crate) queue_type: u32,
    pub(crate) cx: u32,
    pub(crate) cy: u32,
    pub(crate) interval: u64,
    pub(crate) reserved: [u32; 8],
}

impl QueueHeader {
    pub(crate) fn new(width: u32, height: u32, interval: u64, offsets: [u32; 3]) -> Self {
        // C's `struct queue_header header = {0}` initializes the entire object
        // representation before selected fields are assigned. Every all-zero
        // bit pattern is valid for this integer-only Rust representation.
        // SAFETY: All fields are integer scalars/arrays for which zero is valid.
        let mut header = unsafe { core::mem::MaybeUninit::<Self>::zeroed().assume_init() };
        header.state = STATE_STARTING;
        header.offsets = offsets;
        header.cx = width;
        header.cy = height;
        header.interval = interval;
        header
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueueLayout {
    pub(crate) total_size: usize,
    pub(crate) offsets: [u32; SLOT_COUNT],
    pub(crate) frame_size: usize,
}

impl QueueLayout {
    pub(crate) fn calculate(frame_size: usize) -> Result<Self> {
        let mut size = align_32(size_of::<QueueHeader>())?;
        let mut offsets = [0_u32; SLOT_COUNT];
        for offset in &mut offsets {
            *offset = u32::try_from(size).map_err(|_| Error::FrameSizeOverflow)?;
            size = size
                .checked_add(FRAME_HEADER_SIZE)
                .and_then(|value| value.checked_add(frame_size))
                .ok_or(Error::FrameSizeOverflow)?;
            size = align_32(size)?;
        }
        u32::try_from(size).map_err(|_| Error::FrameSizeOverflow)?;
        Ok(Self {
            total_size: size,
            offsets,
            frame_size,
        })
    }
}

fn align_32(size: usize) -> Result<usize> {
    size.checked_add(31)
        .map(|value| value & !31)
        .ok_or(Error::FrameSizeOverflow)
}

const _: () = {
    assert!(size_of::<QueueHeader>() == 80);
    assert!(align_of::<QueueHeader>() == 8);
    assert!(offset_of!(QueueHeader, write_idx) == 0);
    assert!(offset_of!(QueueHeader, read_idx) == 4);
    assert!(offset_of!(QueueHeader, state) == 8);
    assert!(offset_of!(QueueHeader, offsets) == 12);
    assert!(offset_of!(QueueHeader, queue_type) == 24);
    assert!(offset_of!(QueueHeader, cx) == 28);
    assert!(offset_of!(QueueHeader, cy) == 32);
    assert!(offset_of!(QueueHeader, interval) == 40);
    assert!(offset_of!(QueueHeader, reserved) == 48);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_header_matches_msvc_c_layout() {
        assert_eq!(size_of::<QueueHeader>(), 80);
        assert_eq!(align_of::<QueueHeader>(), 8);
        assert_eq!(offset_of!(QueueHeader, interval), 40);
        assert_eq!(offset_of!(QueueHeader, reserved), 48);
    }

    #[test]
    fn three_slots_are_32_byte_aligned() {
        let layout = QueueLayout::calculate(1920 * 1080 * 3 / 2).expect("valid layout");
        assert!(layout.offsets.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(layout.offsets.iter().all(|offset| offset % 32 == 0));
        assert_eq!(layout.total_size % 32, 0);
    }

    #[test]
    fn index_wraparound_matches_unsigned_long_modulo() {
        let wrapped = u32::MAX.wrapping_add(1);
        assert_eq!(wrapped, 0);
        assert_eq!((wrapped % SLOT_COUNT as u32) as usize, 0);
        assert_eq!((u32::MAX % SLOT_COUNT as u32) as usize, 0);
    }
}
