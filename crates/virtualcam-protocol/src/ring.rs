//! Transport v2 frame ring shared by the DirectShow and Media Foundation backends.
//!
//! ```text
//! offset 0     RingHeader (256 bytes, inside a 4 KiB header page)
//! offset 4096  slot 0: SlotHeader (64 bytes) + tightly packed frame
//!              slot 1 ... slot N-1, each `slot_stride` bytes (4 KiB multiple)
//! ```
//!
//! Every slot is guarded by a sequence lock. For frame sequence `s` (starting
//! at 1) the writer uses slot `s % N` and:
//!
//! 1. stores `lock = 2s + 1` (odd: writing),
//! 2. writes the timestamp and payload,
//! 3. stores `lock = 2s` (even: stable),
//! 4. publishes `latest_seq = s`, then `state = ACTIVE` once.
//!
//! A reader observes `latest_seq = s`, requires `lock == 2s` before *and*
//! after copying slot `s % N`, and retries with the newest sequence otherwise.
//! A reader can therefore never accept a frame that the writer overwrote
//! while it was being copied — unlike the OBS three-slot queue, which has no
//! per-slot generation. Writers never wait for readers.

use core::fmt;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::{NonNull, addr_of, addr_of_mut};

use crate::format::{FormatError, ValidatedFormat, VideoFormat, WireVideoFormat};
use crate::sync;

/// `RingHeader::magic`: `"VCR2"`.
pub const RING_MAGIC: u32 = u32::from_le_bytes(*b"VCR2");
/// Current ring layout version.
pub const RING_VERSION: u32 = 1;
/// Producer initialized the ring but has not published a frame.
pub const RING_STATE_STARTING: u32 = 1;
/// At least one frame has been published.
pub const RING_STATE_ACTIVE: u32 = 2;
/// Producer closed the session gracefully.
pub const RING_STATE_STOPPED: u32 = 3;

/// Slot count used unless a caller overrides it.
pub const DEFAULT_SLOT_COUNT: u32 = 3;
/// Smallest accepted slot count.
pub const MIN_SLOT_COUNT: u32 = 2;
/// Largest accepted slot count.
pub const MAX_SLOT_COUNT: u32 = 8;

const HEADER_PAGE: usize = 4096;
const SLOT_ALIGN: usize = 4096;
/// Bytes reserved at the start of every slot; payloads are 64-byte aligned.
pub const SLOT_HEADER_SIZE: usize = 64;
const READ_ATTEMPTS: usize = 4;

/// Fixed ring header at offset 0.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct RingHeader {
    /// [`RING_MAGIC`].
    pub magic: u32,
    /// [`RING_VERSION`].
    pub version: u32,
    /// `size_of::<RingHeader>()`.
    pub header_size: u32,
    /// Number of slots.
    pub slot_count: u32,
    /// Bytes between slot starts.
    pub slot_stride: u64,
    /// Tightly packed payload bytes per frame.
    pub frame_size: u64,
    /// Format of every payload.
    pub format: WireVideoFormat,
    /// `RING_STATE_*`; published.
    pub state: u32,
    /// Writer's process id (diagnostics/liveness hint only).
    pub producer_pid: u32,
    /// Latest committed frame sequence, 0 before the first frame; published.
    pub latest_seq: u64,
    /// Session generation chosen by the ring creator.
    pub generation: u64,
    /// Must be zero.
    pub reserved: [u64; 21],
}

/// Header at the start of every slot.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct SlotHeader {
    /// Sequence lock: `2s + 1` while frame `s` is written, `2s` once stable.
    pub lock: u64,
    /// Capture time of the frame, QPC-derived 100 ns units.
    pub timestamp_100ns: u64,
    /// Must be zero.
    pub reserved: [u64; 6],
}

const _: () = {
    assert!(size_of::<RingHeader>() == 256);
    assert!(align_of::<RingHeader>() == 64);
    assert!(offset_of!(RingHeader, slot_stride) == 16);
    assert!(offset_of!(RingHeader, format) == 32);
    assert!(offset_of!(RingHeader, state) == 64);
    assert!(offset_of!(RingHeader, latest_seq) == 72);
    assert!(offset_of!(RingHeader, generation) == 80);
    assert!(size_of::<SlotHeader>() == SLOT_HEADER_SIZE);
};

/// Ring construction/validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// Slot count outside [`MIN_SLOT_COUNT`]..=[`MAX_SLOT_COUNT`].
    InvalidSlotCount,
    /// Layout arithmetic overflowed.
    Overflow,
    /// The mapped region is smaller than the layout or misaligned.
    RegionTooSmall,
    /// Header magic/version/sizes are not a v2 ring.
    InvalidHeader,
    /// The ring's format is malformed.
    Format(FormatError),
    /// The ring carries a different format than expected.
    FormatMismatch,
}

impl fmt::Display for RingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSlotCount => f.write_str("invalid ring slot count"),
            Self::Overflow => f.write_str("ring layout overflow"),
            Self::RegionTooSmall => f.write_str("ring mapping is too small or misaligned"),
            Self::InvalidHeader => f.write_str("ring header is not a transport v2 ring"),
            Self::Format(error) => write!(f, "ring format is invalid: {error}"),
            Self::FormatMismatch => f.write_str("ring format differs from the expected format"),
        }
    }
}

impl From<FormatError> for RingError {
    fn from(error: FormatError) -> Self {
        Self::Format(error)
    }
}

/// Byte geometry of a ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    /// Number of slots.
    pub slot_count: u32,
    /// Bytes between slot starts.
    pub slot_stride: usize,
    /// Payload bytes per frame.
    pub frame_size: usize,
    /// Total mapping size.
    pub total_size: usize,
}

impl RingLayout {
    /// Computes the layout for `frame_size`-byte frames in `slot_count` slots.
    pub fn new(frame_size: usize, slot_count: u32) -> Result<Self, RingError> {
        if !(MIN_SLOT_COUNT..=MAX_SLOT_COUNT).contains(&slot_count) {
            return Err(RingError::InvalidSlotCount);
        }
        let slot_stride = SLOT_HEADER_SIZE.checked_add(frame_size).and_then(|v| v.checked_add(SLOT_ALIGN - 1)).ok_or(RingError::Overflow)? & !(SLOT_ALIGN - 1);
        let total_size = slot_stride.checked_mul(slot_count as usize).and_then(|v| v.checked_add(HEADER_PAGE)).ok_or(RingError::Overflow)?;
        Ok(Self {
            slot_count,
            slot_stride,
            frame_size,
            total_size,
        })
    }

    /// Computes the layout for `format`.
    pub fn for_format(format: &ValidatedFormat, slot_count: u32) -> Result<Self, RingError> {
        Self::new(format.frame_size(), slot_count)
    }

    const fn slot_offset(&self, sequence: u64) -> usize {
        HEADER_PAGE + (sequence % self.slot_count as u64) as usize * self.slot_stride
    }
}

fn check_region(base: *const u8, len: usize, layout: &RingLayout) -> Result<NonNull<u8>, RingError> {
    let base = NonNull::new(base.cast_mut()).ok_or(RingError::RegionTooSmall)?;
    if len < layout.total_size || base.as_ptr().align_offset(64) != 0 {
        return Err(RingError::RegionTooSmall);
    }
    Ok(base)
}

/// Single-writer half of a ring.
pub struct RingWriter {
    base: NonNull<u8>,
    layout: RingLayout,
    next_seq: u64,
    open_seq: Option<u64>,
    published_active: bool,
}

// SAFETY: The writer owns exclusive write access to a shared mapping; the
// mapping itself has no thread affinity.
unsafe impl Send for RingWriter {}

impl RingWriter {
    /// Initializes a fresh ring and publishes [`RING_STATE_STARTING`].
    ///
    /// # Safety
    ///
    /// `base..base + len` must be writable shared memory that stays mapped for
    /// the writer's lifetime, and no other writer may use it concurrently.
    pub unsafe fn initialize(base: *mut u8, len: usize, format: &ValidatedFormat, slot_count: u32, generation: u64, producer_pid: u32) -> Result<Self, RingError> {
        let layout = RingLayout::for_format(format, slot_count)?;
        let base = check_region(base, len, &layout)?;
        let header = base.as_ptr().cast::<RingHeader>();
        let initial = RingHeader {
            magic: RING_MAGIC,
            version: RING_VERSION,
            header_size: size_of::<RingHeader>() as u32,
            slot_count,
            slot_stride: layout.slot_stride as u64,
            frame_size: layout.frame_size as u64,
            format: format.format().to_wire()?,
            state: 0,
            producer_pid,
            latest_seq: 0,
            generation,
            reserved: [0; 21],
        };
        // SAFETY: The caller guarantees exclusive writable access to the
        // checked region. The state is invalidated before metadata changes and
        // STARTING is published only after every slot lock is reset.
        unsafe {
            sync::publish_u32(addr_of_mut!((*header).state), 0);
            core::ptr::write(header, initial);
            for index in 0..u64::from(slot_count) {
                let slot = base.as_ptr().add(layout.slot_offset(index)).cast::<SlotHeader>();
                core::ptr::write(
                    slot,
                    SlotHeader {
                        lock: 0,
                        timestamp_100ns: 0,
                        reserved: [0; 6],
                    },
                );
            }
            sync::publish_u32(addr_of_mut!((*header).state), RING_STATE_STARTING);
        }
        Ok(Self {
            base,
            layout,
            next_seq: 1,
            open_seq: None,
            published_active: false,
        })
    }

    /// Attaches as the writer of a ring another process initialized (the
    /// Media Foundation source creates rings; the producer writes them).
    ///
    /// # Safety
    ///
    /// Same as [`Self::initialize`]; the region must hold an initialized ring.
    pub unsafe fn attach(base: *mut u8, len: usize, expected: &ValidatedFormat) -> Result<Self, RingError> {
        // SAFETY: Forwarded caller contract; `open` validates the header.
        let reader = unsafe { RingReader::open(base, len) }?;
        if reader.format() != expected.format() {
            return Err(RingError::FormatMismatch);
        }
        let next_seq = reader.latest_sequence().wrapping_add(1).max(1);
        Ok(Self {
            base: reader.base,
            layout: reader.layout,
            next_seq,
            open_seq: None,
            published_active: false,
        })
    }

    /// Payload bytes per frame.
    pub const fn frame_size(&self) -> usize {
        self.layout.frame_size
    }

    /// Total bytes of the ring.
    pub const fn layout(&self) -> RingLayout {
        self.layout
    }

    fn header(&self) -> *mut RingHeader {
        self.base.as_ptr().cast()
    }

    fn slot(&self, sequence: u64) -> *mut SlotHeader {
        // SAFETY: `slot_offset` is inside the region checked at construction.
        unsafe { self.base.as_ptr().add(self.layout.slot_offset(sequence)).cast() }
    }

    /// Locks the next slot and returns its payload pointer (`frame_size`
    /// writable bytes, 64-byte aligned). Must be followed by
    /// [`Self::commit_frame`] or [`Self::abort_frame`].
    pub fn begin_frame(&mut self) -> *mut u8 {
        if let Some(sequence) = self.open_seq.take() {
            self.release(sequence);
        }
        let sequence = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        let slot = self.slot(sequence);
        // SAFETY: `slot` is an aligned slot header inside the mapped region.
        unsafe { sync::publish_u64(addr_of_mut!((*slot).lock), sequence.wrapping_mul(2) | 1) };
        self.open_seq = Some(sequence);
        // SAFETY: The payload follows the slot header inside the same slot.
        unsafe { slot.cast::<u8>().add(SLOT_HEADER_SIZE) }
    }

    /// Stamps and publishes the frame opened by [`Self::begin_frame`].
    /// Returns its sequence, or `None` if no frame was open.
    pub fn commit_frame(&mut self, timestamp_100ns: u64) -> Option<u64> {
        let sequence = self.open_seq.take()?;
        let slot = self.slot(sequence);
        let header = self.header();
        // SAFETY: Live aligned slot/header fields. The timestamp is written
        // while the lock is still odd; each publish is a full barrier, so the
        // payload is visible before the even lock and the lock before
        // `latest_seq`.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*slot).timestamp_100ns), timestamp_100ns);
            sync::publish_u64(addr_of_mut!((*slot).lock), sequence.wrapping_mul(2));
            sync::publish_u64(addr_of_mut!((*header).latest_seq), sequence);
            if !self.published_active {
                self.published_active = true;
                sync::publish_u32(addr_of_mut!((*header).state), RING_STATE_ACTIVE);
            }
        }
        Some(sequence)
    }

    /// Abandons the open frame without publishing it. Readers of the slot's
    /// previous frame detect the lock change and retry with a newer frame.
    pub fn abort_frame(&mut self) {
        if let Some(sequence) = self.open_seq.take() {
            self.release(sequence);
        }
    }

    fn release(&self, sequence: u64) {
        let slot = self.slot(sequence);
        // SAFETY: Live aligned slot header. An even lock that `latest_seq`
        // never names is never accepted by a reader.
        unsafe { sync::publish_u64(addr_of_mut!((*slot).lock), sequence.wrapping_mul(2)) };
    }

    /// Publishes [`RING_STATE_STOPPED`].
    pub fn stop(&mut self) {
        self.abort_frame();
        // SAFETY: Live aligned header field.
        unsafe { sync::publish_u32(addr_of_mut!((*self.header()).state), RING_STATE_STOPPED) };
    }
}

/// Result of [`RingReader::read_latest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// A stable frame was copied.
    Frame {
        /// Frame sequence (strictly increasing per ring).
        sequence: u64,
        /// Producer capture timestamp.
        timestamp_100ns: u64,
    },
    /// No frame has been published yet.
    Empty,
    /// The writer lapped every attempt; the destination may hold a torn copy.
    Contended,
}

/// Read-only half of a ring; any number of readers may coexist.
pub struct RingReader {
    base: NonNull<u8>,
    layout: RingLayout,
    format: VideoFormat,
    generation: u64,
}

// SAFETY: Readers only perform atomic observations and plain copies of a
// mapping that has no thread affinity.
unsafe impl Send for RingReader {}

impl RingReader {
    /// Validates an initialized ring header.
    ///
    /// Every size is recomputed from the declared format; header-supplied
    /// sizes are only cross-checked, so a corrupt peer cannot cause
    /// out-of-bounds access.
    ///
    /// # Safety
    ///
    /// `base..base + len` must stay mapped and readable for the reader's lifetime.
    pub unsafe fn open(base: *const u8, len: usize) -> Result<Self, RingError> {
        if len < HEADER_PAGE {
            return Err(RingError::RegionTooSmall);
        }
        let head = NonNull::new(base.cast_mut()).ok_or(RingError::RegionTooSmall)?;
        if head.as_ptr().align_offset(64) != 0 {
            return Err(RingError::RegionTooSmall);
        }
        let header = head.as_ptr().cast::<RingHeader>();
        // SAFETY: The header page is inside the caller's region. Observing the
        // state orders the following metadata reads after it.
        let snapshot = unsafe {
            let state = sync::observe_u32(addr_of!((*header).state));
            if !matches!(state, RING_STATE_STARTING | RING_STATE_ACTIVE | RING_STATE_STOPPED) {
                return Err(RingError::InvalidHeader);
            }
            core::ptr::read_volatile(header)
        };
        if snapshot.magic != RING_MAGIC || snapshot.version != RING_VERSION || snapshot.header_size as usize != size_of::<RingHeader>() {
            return Err(RingError::InvalidHeader);
        }
        let format = snapshot.format.decode()?;
        let validated = format.validate()?;
        let layout = RingLayout::for_format(&validated, snapshot.slot_count)?;
        if snapshot.slot_stride != layout.slot_stride as u64 || snapshot.frame_size != layout.frame_size as u64 {
            return Err(RingError::InvalidHeader);
        }
        let base = check_region(base, len, &layout)?;
        Ok(Self {
            base,
            layout,
            format,
            generation: snapshot.generation,
        })
    }

    /// Format of every frame in this ring.
    pub const fn format(&self) -> VideoFormat {
        self.format
    }

    /// Ring geometry.
    pub const fn layout(&self) -> RingLayout {
        self.layout
    }

    /// Session generation stamped by the ring creator.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    fn header(&self) -> *const RingHeader {
        self.base.as_ptr().cast()
    }

    /// Current `RING_STATE_*`.
    pub fn state(&self) -> u32 {
        // SAFETY: Live aligned header field inside the validated region.
        unsafe { sync::observe_u32(addr_of!((*self.header()).state)) }
    }

    /// Latest committed sequence, 0 before the first frame.
    pub fn latest_sequence(&self) -> u64 {
        // SAFETY: Live aligned header field inside the validated region.
        unsafe { sync::observe_u64(addr_of!((*self.header()).latest_seq)) }
    }

    /// Writer process id recorded in the header.
    pub fn producer_pid(&self) -> u32 {
        // SAFETY: Live aligned header field inside the validated region.
        unsafe { core::ptr::read_volatile(addr_of!((*self.header()).producer_pid)) }
    }

    /// Copies the newest stable frame.
    ///
    /// `copy` receives a pointer to `frame_size` payload bytes. The bytes may
    /// change concurrently, so `copy` must only use raw-pointer copies
    /// (`ptr::copy_nonoverlapping`) and never form references into them; it
    /// may be called several times, each time overwriting its destination.
    /// After a [`ReadOutcome::Contended`] result the destination holds the
    /// last (possibly torn) attempt.
    pub fn read_latest(&self, mut copy: impl FnMut(*const u8)) -> ReadOutcome {
        for _ in 0..READ_ATTEMPTS {
            let sequence = self.latest_sequence();
            if sequence == 0 {
                return ReadOutcome::Empty;
            }
            let expected = sequence.wrapping_mul(2);
            // SAFETY: `slot_offset` lies inside the validated region.
            let slot = unsafe { self.base.as_ptr().add(self.layout.slot_offset(sequence)).cast::<SlotHeader>() };
            // SAFETY: Live aligned slot header field.
            if unsafe { sync::observe_u64(addr_of!((*slot).lock)) } != expected {
                continue;
            }
            // SAFETY: As above; the payload follows the slot header.
            let timestamp_100ns = unsafe { core::ptr::read_volatile(addr_of!((*slot).timestamp_100ns)) };
            // SAFETY: Payload pointer inside the validated slot.
            copy(unsafe { slot.cast::<u8>().add(SLOT_HEADER_SIZE) });
            sync::read_barrier();
            // SAFETY: Live aligned slot header field.
            if unsafe { sync::observe_u64(addr_of!((*slot).lock)) } == expected {
                return ReadOutcome::Frame { sequence, timestamp_100ns };
            }
        }
        ReadOutcome::Contended
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::format::{FrameRate, PixelFormat};

    #[repr(C, align(4096))]
    struct Page([u8; 4096]);

    struct Region(Vec<Page>);

    impl Region {
        fn new(bytes: usize) -> Self {
            Self((0..bytes.div_ceil(4096)).map(|_| Page([0; 4096])).collect())
        }
        fn ptr(&mut self) -> *mut u8 {
            self.0.as_mut_ptr().cast()
        }
        fn len(&self) -> usize {
            self.0.len() * 4096
        }
    }

    fn format(width: u32, height: u32) -> ValidatedFormat {
        VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(30)).validate().expect("valid")
    }

    fn copy_into(dst: &mut [u8]) -> impl FnMut(*const u8) + '_ {
        move |src| {
            // SAFETY: Test payloads are exactly `dst.len()` bytes.
            unsafe { core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len()) }
        }
    }

    #[test]
    fn layout_is_page_aligned_and_bounded() {
        let layout = RingLayout::new(3_110_400, 3).expect("layout");
        assert_eq!(layout.slot_stride % 4096, 0);
        assert!(layout.slot_stride >= 3_110_400 + SLOT_HEADER_SIZE);
        assert_eq!(layout.total_size, 4096 + 3 * layout.slot_stride);
        assert_eq!(RingLayout::new(1, 1), Err(RingError::InvalidSlotCount));
        assert_eq!(RingLayout::new(1, 9), Err(RingError::InvalidSlotCount));
        assert_eq!(RingLayout::new(usize::MAX, 3), Err(RingError::Overflow));
    }

    #[test]
    fn writer_publishes_and_reader_copies_latest() {
        let format = format(4, 2);
        let layout = RingLayout::for_format(&format, 3).expect("layout");
        let mut region = Region::new(layout.total_size);
        let (ptr, len) = (region.ptr(), region.len());
        // SAFETY: The region outlives writer and reader in this test.
        let mut writer = unsafe { RingWriter::initialize(ptr, len, &format, 3, 0xABCD, 42) }.expect("init");
        // SAFETY: As above.
        let reader = unsafe { RingReader::open(ptr, len) }.expect("open");
        assert_eq!((reader.state(), reader.generation(), reader.producer_pid()), (RING_STATE_STARTING, 0xABCD, 42));
        assert_eq!(reader.format(), format.format());
        let mut out = [0_u8; 12];
        assert_eq!(reader.read_latest(copy_into(&mut out)), ReadOutcome::Empty);

        for value in 1..=5_u8 {
            let payload = writer.begin_frame();
            // SAFETY: The payload has `frame_size` writable bytes.
            unsafe { core::ptr::write_bytes(payload, value, writer.frame_size()) };
            assert_eq!(writer.commit_frame(u64::from(value) * 10), Some(u64::from(value)));
        }
        assert_eq!(reader.state(), RING_STATE_ACTIVE);
        assert_eq!(reader.read_latest(copy_into(&mut out)), ReadOutcome::Frame { sequence: 5, timestamp_100ns: 50 });
        assert_eq!(out, [5; 12]);

        writer.stop();
        assert_eq!(reader.state(), RING_STATE_STOPPED);
    }

    #[test]
    fn open_slot_is_never_read_and_abort_keeps_previous_frame() {
        let format = format(4, 2);
        let layout = RingLayout::for_format(&format, 2).expect("layout");
        let mut region = Region::new(layout.total_size);
        let (ptr, len) = (region.ptr(), region.len());
        // SAFETY: The region outlives writer and reader.
        let mut writer = unsafe { RingWriter::initialize(ptr, len, &format, 2, 1, 1) }.expect("init");
        // SAFETY: As above.
        let reader = unsafe { RingReader::open(ptr, len) }.expect("open");
        let mut out = [0_u8; 12];

        let payload = writer.begin_frame();
        // SAFETY: Writable payload.
        unsafe { core::ptr::write_bytes(payload, 1, 12) };
        writer.commit_frame(1);
        // Frame 2 is open (slot 0) while frame 1 (slot 1) stays readable.
        let payload = writer.begin_frame();
        // SAFETY: Writable payload.
        unsafe { core::ptr::write_bytes(payload, 2, 12) };
        assert_eq!(reader.read_latest(copy_into(&mut out)), ReadOutcome::Frame { sequence: 1, timestamp_100ns: 1 });
        assert_eq!(out, [1; 12]);
        writer.abort_frame();
        assert_eq!(reader.read_latest(copy_into(&mut out)), ReadOutcome::Frame { sequence: 1, timestamp_100ns: 1 });

        // Frame 3 reuses slot 1: a reader that already chose frame 1 sees the
        // lock move to odd and refuses it.
        let payload = writer.begin_frame();
        let mut calls = 0;
        let outcome = reader.read_latest(|_| calls += 1);
        assert_eq!((outcome, calls), (ReadOutcome::Contended, 0));
        // SAFETY: Writable payload.
        unsafe { core::ptr::write_bytes(payload, 3, 12) };
        writer.commit_frame(3);
        assert_eq!(reader.read_latest(copy_into(&mut out)), ReadOutcome::Frame { sequence: 3, timestamp_100ns: 3 });
        assert_eq!(out, [3; 12]);
    }

    #[test]
    fn attach_continues_sequence_and_checks_format() {
        let format = format(4, 2);
        let layout = RingLayout::for_format(&format, 3).expect("layout");
        let mut region = Region::new(layout.total_size);
        let (ptr, len) = (region.ptr(), region.len());
        // SAFETY: The region outlives every writer.
        let mut first = unsafe { RingWriter::initialize(ptr, len, &format, 3, 9, 1) }.expect("init");
        first.begin_frame();
        first.commit_frame(1);
        let _ = first;
        // SAFETY: As above.
        assert!(matches!(unsafe { RingWriter::attach(ptr, len, &self::format(8, 2)) }, Err(RingError::FormatMismatch)));
        // SAFETY: As above.
        let mut second = unsafe { RingWriter::attach(ptr, len, &format) }.expect("attach");
        second.begin_frame();
        assert_eq!(second.commit_frame(2), Some(2));
    }

    #[test]
    fn reader_rejects_corrupt_headers() {
        let format = format(4, 2);
        let layout = RingLayout::for_format(&format, 3).expect("layout");
        let mut region = Region::new(layout.total_size);
        let (ptr, len) = (region.ptr(), region.len());
        // SAFETY: Region outlives the writer.
        unsafe { RingWriter::initialize(ptr, len, &format, 3, 1, 1) }.expect("init");
        // SAFETY: Region is live.
        assert!(unsafe { RingReader::open(ptr, layout.total_size - 1) }.is_err());
        let header = ptr.cast::<RingHeader>();
        // SAFETY: Region is live and exclusively owned by this test.
        unsafe {
            (*header).slot_stride += 4096;
            assert!(matches!(RingReader::open(ptr, len), Err(RingError::InvalidHeader)));
            (*header).slot_stride -= 4096;
            (*header).format.width = 0;
            assert!(matches!(RingReader::open(ptr, len), Err(RingError::Format(_))));
            (*header).format.width = 4;
            (*header).state = 77;
            assert!(matches!(RingReader::open(ptr, len), Err(RingError::InvalidHeader)));
        }
    }

    /// The torn-frame stress that the OBS queue fails: an unthrottled writer
    /// against a deliberately slow reader. Every accepted frame must be
    /// uniform.
    #[test]
    fn seqlock_never_accepts_torn_frames_under_contention() {
        let format = format(64, 64);
        let frame = format.frame_size();
        let layout = RingLayout::for_format(&format, 3).expect("layout");
        let region = Arc::new(std::sync::Mutex::new(Region::new(layout.total_size)));
        let (ptr, len) = {
            let mut guard = region.lock().expect("region");
            (guard.ptr() as usize, guard.len())
        };
        // SAFETY: `region` is kept alive by the Arc until both threads join.
        let mut writer = unsafe { RingWriter::initialize(ptr as *mut u8, len, &format, 3, 1, 1) }.expect("init");
        // SAFETY: As above.
        let reader = unsafe { RingReader::open(ptr as *const u8, len) }.expect("open");
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut value = 0_u8;
            while !writer_stop.load(Ordering::Relaxed) {
                value = value.wrapping_add(1);
                let payload = writer.begin_frame();
                // SAFETY: Writable payload of `frame` bytes.
                unsafe { core::ptr::write_bytes(payload, value, frame) };
                writer.commit_frame(u64::from(value));
            }
        });

        let mut out = vec![0_u8; frame];
        let (mut accepted, mut contended) = (0, 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while std::time::Instant::now() < deadline {
            let outcome = reader.read_latest(|src| {
                // Copy in small chunks with yields so the writer laps us.
                for offset in (0..frame).step_by(512) {
                    // SAFETY: In-bounds raw copy of a concurrently written payload.
                    unsafe { core::ptr::copy_nonoverlapping(src.add(offset), out.as_mut_ptr().add(offset), 512.min(frame - offset)) };
                    std::thread::yield_now();
                }
            });
            match outcome {
                ReadOutcome::Frame { .. } => {
                    accepted += 1;
                    assert!(out.iter().all(|byte| *byte == out[0]), "accepted a torn frame");
                }
                ReadOutcome::Contended => contended += 1,
                ReadOutcome::Empty => {}
            }
        }
        stop.store(true, Ordering::Relaxed);
        handle.join().expect("writer thread");
        drop(region);
        assert!(accepted + contended > 0);
    }
}
