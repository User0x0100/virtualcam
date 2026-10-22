//! OBS-compatible three-slot NV12 frame queue.
//!
//! OBS Virtual Camera and the managed DirectShow camera share this payload
//! ABI. Ownership, naming, and lifecycle stay with each backend's control
//! plane; only the queue layout and its publication order live here.

use core::mem::{align_of, offset_of, size_of};

use crate::{Error, Result};

pub(crate) const STATE_STARTING: u32 = 1;
pub(crate) const STATE_READY: u32 = 2;
pub(crate) const STATE_STOPPING: u32 = 3;
pub(crate) const FRAME_HEADER_SIZE: usize = 32;
pub(crate) const SLOT_COUNT: usize = 3;

/// Mirrors `struct queue_header` from obsproject/obs-studio@ca937366c1e9fc7f22fd5f1c7d3a3a5f7951170b,
/// `shared/obs-shared-memory-queue/shared-memory-queue.c`.
///
/// The three leading `uint32_t` fields are volatile in C. Rust keeps their ABI
/// as plain integers and accesses them only through `protocol::publication`.
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
    /// Builds a header whose state is still unpublished (zero). Writers copy it
    /// into the mapping first and publish `STATE_STARTING` afterwards.
    pub(crate) const fn unpublished(width: u32, height: u32, interval: u64, offsets: [u32; SLOT_COUNT]) -> Self {
        Self {
            write_idx: 0,
            read_idx: 0,
            state: 0,
            offsets,
            queue_type: 0,
            cx: width,
            cy: height,
            interval,
            reserved: [0; 8],
        }
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
            size = size.checked_add(FRAME_HEADER_SIZE).and_then(|value| value.checked_add(frame_size)).ok_or(Error::FrameSizeOverflow)?;
            size = align_32(size)?;
        }
        u32::try_from(size).map_err(|_| Error::FrameSizeOverflow)?;
        Ok(Self { total_size: size, offsets, frame_size })
    }

    /// Returns each slot's byte offset as `usize`.
    pub(crate) fn slot_offsets(&self) -> [usize; SLOT_COUNT] {
        // u32 -> usize is lossless on every supported Windows target.
        self.offsets.map(|offset| offset as usize)
    }
}

/// Maps a published sequence number to its slot exactly like upstream's
/// `(unsigned long)inc % 3`.
pub(crate) const fn slot_index(sequence: u32) -> usize {
    (sequence % SLOT_COUNT as u32) as usize
}

fn align_32(size: usize) -> Result<usize> {
    size.checked_add(31).map(|value| value & !31).ok_or(Error::FrameSizeOverflow)
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
    fn tiny_layout_has_exact_protocol_offsets() {
        let layout = QueueLayout::calculate(1).expect("valid layout");
        assert_eq!(layout.offsets, [96, 160, 224]);
        assert_eq!(layout.slot_offsets(), [96, 160, 224]);
        assert_eq!(layout.total_size, 288);
        assert_eq!(layout.frame_size, 1);
    }

    #[test]
    fn layout_rejects_size_overflow() {
        assert_eq!(QueueLayout::calculate(usize::MAX), Err(Error::FrameSizeOverflow));
    }

    #[test]
    fn unpublished_header_initializes_protocol_metadata() {
        let header = QueueHeader::unpublished(640, 480, 333_333, [96, 128, 160]);
        assert_eq!(header.write_idx, 0);
        assert_eq!(header.read_idx, 0);
        assert_eq!(header.state, 0);
        assert_eq!(header.offsets, [96, 128, 160]);
        assert_eq!(header.queue_type, 0);
        assert_eq!((header.cx, header.cy), (640, 480));
        assert_eq!(header.interval, 333_333);
        assert_eq!(header.reserved, [0; 8]);
    }

    #[test]
    fn index_wraparound_matches_unsigned_long_modulo() {
        assert_eq!(slot_index(u32::MAX.wrapping_add(1)), 0);
        assert_eq!(slot_index(u32::MAX), 0);
        assert_eq!(slot_index(4), 1);
    }
}
