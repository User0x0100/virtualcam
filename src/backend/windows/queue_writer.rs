//! Producer side of the OBS-compatible NV12 frame queue.
//!
//! OBS Virtual Camera and the managed DirectShow camera both write this queue;
//! they differ only in naming, occupancy, and control-plane lifecycle, which
//! stay in their own backend modules.

use core::ptr::addr_of_mut;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};

use super::FrameSource;
use super::protocol::frame_queue::{FRAME_HEADER_SIZE, QueueHeader, QueueLayout, SLOT_COUNT, STATE_READY, STATE_STARTING, STATE_STOPPING, slot_index};
use super::protocol::publication;
use crate::config::Nv12Format;
use crate::platform::windows::{MappedView, OwnedHandle, QpcClock, opened_existing_object};
use crate::{CameraType, Error, Result};

/// How [`QueueWriter::create`] treats a mapping that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingQueue {
    /// An existing mapping means another producer owns the queue.
    Reject,
    /// Reinitialize an existing mapping that is large enough for the layout.
    Reinitialize,
}

pub(super) struct QueueWriter {
    // Declared before the mapping handle so the view is unmapped first.
    view: MappedView,
    _mapping: OwnedHandle,
    slot_offsets: [usize; SLOT_COUNT],
    frame_size: usize,
    clock: QpcClock,
    stopped: bool,
}

impl QueueWriter {
    /// Creates or reinitializes the named queue and publishes `STATE_STARTING`.
    ///
    /// `name` must be NUL-terminated UTF-16.
    pub(super) fn create(name: &[u16], format: Nv12Format, existing: ExistingQueue, backend: CameraType) -> Result<Self> {
        let layout = QueueLayout::calculate(format.frame_size)?;
        let mapping_size = u32::try_from(layout.total_size).map_err(|_| Error::FrameSizeOverflow)?;
        // Acquire the clock before touching shared state so a failure cannot
        // leave a half-published queue behind.
        let clock = QpcClock::new()?;

        // SAFETY: INVALID_HANDLE_VALUE selects a page-file-backed mapping; the
        // protection, 32-bit size, and NUL-terminated name match upstream.
        let raw = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null(), PAGE_READWRITE, 0, mapping_size, name.as_ptr()) };
        // SAFETY: A non-null CreateFileMappingW result is one owned handle.
        let mapping = unsafe { OwnedHandle::from_result(raw, "CreateFileMappingW(NV12 frame queue)") }?;
        let reused = opened_existing_object();
        if reused && existing == ExistingQueue::Reject {
            return Err(Error::DeviceInUse);
        }

        let view = MappedView::map(mapping.raw(), FILE_MAP_ALL_ACCESS, "MapViewOfFile(NV12 frame queue)")?;
        if view.accessible_len() < layout.total_size {
            return Err(if reused {
                Error::ExistingMappingTooSmall {
                    required: layout.total_size,
                    available: view.accessible_len(),
                }
            } else {
                Error::Protocol {
                    backend,
                    detail: "new NV12 queue mapping exposes less memory than requested",
                }
            });
        }
        let header = view.ptr_at::<QueueHeader>(0).ok_or(Error::Protocol {
            backend,
            detail: "NV12 queue mapping does not provide an aligned queue header",
        })?;
        let slot_offsets = layout.slot_offsets();
        // `layout.total_size` already covers every slot's 32-byte frame header
        // and payload; the slot timestamp additionally needs u64 alignment.
        if slot_offsets.iter().any(|offset| view.ptr_at::<u64>(*offset).is_none()) {
            return Err(Error::Protocol {
                backend,
                detail: "NV12 queue slot timestamp is outside or misaligned in the mapping",
            });
        }

        let initial = QueueHeader::unpublished(format.width, format.height, format.interval_100ns, layout.offsets);
        // SAFETY: `header` is checked, aligned, writable storage inside `view`.
        // Invalidating the state first hides any stale reused session before its
        // metadata changes; STARTING is published only after every field is set.
        unsafe {
            let header = header.as_ptr();
            publication::publish_u32(addr_of_mut!((*header).state), 0);
            core::ptr::copy_nonoverlapping(&raw const initial, header, 1);
            publication::publish_u32(addr_of_mut!((*header).state), STATE_STARTING);
        }
        if reused {
            trace_warn!(required = layout.total_size, available = view.accessible_len(), camera_type = ?backend, "NV12 producer reinitialized an existing mapping");
        }

        Ok(Self {
            view,
            _mapping: mapping,
            slot_offsets,
            frame_size: format.frame_size,
            clock,
            stopped: false,
        })
    }

    /// Copies one frame into the next slot and publishes it.
    ///
    /// Publication order mirrors upstream `video_queue_write`: advance
    /// `write_idx`, write the timestamp and payload, then publish `read_idx`
    /// followed by `STATE_READY`.
    pub(super) fn write(&mut self, frame: FrameSource<'_>) -> Result<()> {
        if frame.byte_len() != self.frame_size {
            return Err(Error::FrameSizeMismatch {
                expected: self.frame_size,
                actual: frame.byte_len(),
            });
        }
        if self.stopped {
            return Err(Error::AlreadyClosed);
        }
        let timestamp_ns = self.clock.now_ns()?;
        let header = self.header();

        // SAFETY: `header` is the live aligned queue header validated by `create`.
        let sequence = unsafe { publication::increment_u32(addr_of_mut!((*header).write_idx)) };
        let slot = self.slot_offsets[slot_index(sequence)];
        // SAFETY: `create` proved the slot timestamp is aligned and that the
        // whole frame header plus payload lies inside the writable view.
        let payload = unsafe {
            let slot = self.view.as_ptr().add(slot);
            slot.cast::<u64>().write(timestamp_ns);
            slot.add(FRAME_HEADER_SIZE)
        };
        frame.copy_to(payload)?;

        // SAFETY: Same live header. Each publish is a full barrier, so the
        // timestamp and payload are visible before `read_idx`, and `read_idx`
        // before READY.
        unsafe {
            publication::publish_u32(addr_of_mut!((*header).read_idx), sequence);
            publication::publish_u32(addr_of_mut!((*header).state), STATE_READY);
        }
        Ok(())
    }

    /// Publishes `STATE_STOPPING`. Idempotent; also performed on drop.
    pub(super) fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // SAFETY: The view stays mapped until `self` is dropped.
        unsafe {
            publication::publish_u32(addr_of_mut!((*self.header()).state), STATE_STOPPING);
        }
    }

    fn header(&self) -> *mut QueueHeader {
        self.view.as_ptr().cast()
    }
}

impl Drop for QueueWriter {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use core::ptr::addr_of;

    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};

    use super::*;
    use crate::FrameRate;
    use crate::platform::windows::wide_nul;

    pub(crate) fn test_name(scope: &str, suffix: &str) -> Vec<u16> {
        wide_nul(&format!("Local\\VirtualCamRust{scope}_{}_{suffix}", std::process::id()))
    }

    pub(crate) fn nv12(width: u32, height: u32) -> Nv12Format {
        Nv12Format::new(width, height, FrameRate::default()).expect("valid NV12 test format")
    }

    /// A consumer-style read-only view of a queue, mapped at a distinct address.
    pub(crate) struct QueueObserver {
        view: MappedView,
        _mapping: OwnedHandle,
    }

    impl QueueObserver {
        pub(crate) fn open(name: &[u16]) -> Self {
            // SAFETY: `name` is NUL-terminated; the result is checked below.
            let raw = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
            // SAFETY: A non-null OpenFileMappingW result is one owned handle.
            let mapping = unsafe { OwnedHandle::from_result(raw, "OpenFileMappingW(test observer)") }.expect("queue mapping exists");
            let view = MappedView::map(mapping.raw(), FILE_MAP_READ, "MapViewOfFile(test observer)").expect("map queue read-only");
            Self { view, _mapping: mapping }
        }

        fn header(&self) -> *const QueueHeader {
            self.view.as_ptr().cast()
        }

        pub(crate) fn state(&self) -> u32 {
            // SAFETY: The observer view maps the live initialized header.
            unsafe { publication::observe_u32(addr_of!((*self.header()).state)) }
        }

        pub(crate) fn read_idx(&self) -> u32 {
            // SAFETY: As above.
            unsafe { publication::observe_u32(addr_of!((*self.header()).read_idx)) }
        }

        pub(crate) fn write_idx(&self) -> u32 {
            // SAFETY: As above.
            unsafe { publication::observe_u32(addr_of!((*self.header()).write_idx)) }
        }

        pub(crate) fn metadata(&self) -> (u32, u32, u64) {
            // SAFETY: Metadata is immutable after STARTING was published.
            unsafe {
                let header = self.header();
                ((*header).cx, (*header).cy, (*header).interval)
            }
        }

        pub(crate) fn slot(&self, sequence: u32, frame_size: usize) -> (u64, &[u8]) {
            // SAFETY: Offsets were published before STARTING and the writer's
            // layout keeps timestamp and payload inside the shared mapping.
            unsafe {
                let offset = (*self.header()).offsets[slot_index(sequence)] as usize;
                let slot = self.view.as_ptr().add(offset);
                (slot.cast::<u64>().read(), core::slice::from_raw_parts(slot.add(FRAME_HEADER_SIZE), frame_size))
            }
        }

        /// Test-only writable alias used to model an external writer.
        pub(crate) fn force_write_idx(name: &[u16], value: u32) {
            use windows_sys::Win32::System::Memory::FILE_MAP_WRITE;
            // SAFETY: `name` is NUL-terminated; the result is checked below.
            let raw = unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, name.as_ptr()) };
            // SAFETY: A non-null OpenFileMappingW result is one owned handle.
            let mapping = unsafe { OwnedHandle::from_result(raw, "OpenFileMappingW(test writer)") }.expect("queue mapping exists");
            let view = MappedView::map(mapping.raw(), FILE_MAP_WRITE, "MapViewOfFile(test writer)").expect("map queue writable");
            // SAFETY: The writable alias maps the live aligned header.
            unsafe { publication::publish_u32(addr_of_mut!((*view.as_ptr().cast::<QueueHeader>()).write_idx), value) };
        }
    }

    #[test]
    fn create_publishes_metadata_then_starting_and_reject_reports_in_use() {
        let name = test_name("Queue", "create");
        let format = nv12(4, 2);
        let mut writer = QueueWriter::create(&name, format, ExistingQueue::Reject, CameraType::Obs).expect("create queue");
        let observer = QueueObserver::open(&name);
        assert_eq!(observer.state(), STATE_STARTING);
        assert_eq!(observer.metadata(), (4, 2, 333_333));
        assert_eq!((observer.write_idx(), observer.read_idx()), (0, 0));

        assert!(matches!(QueueWriter::create(&name, format, ExistingQueue::Reject, CameraType::Obs), Err(Error::DeviceInUse)));

        writer.stop();
        assert_eq!(observer.state(), STATE_STOPPING);
        assert_eq!(writer.write(FrameSource::Host(&[0; 12])), Err(Error::AlreadyClosed));
    }

    #[test]
    fn write_publishes_timestamp_payload_index_and_ready_in_order() {
        let name = test_name("Queue", "write");
        let format = nv12(2, 2);
        let mut writer = QueueWriter::create(&name, format, ExistingQueue::Reject, CameraType::Obs).expect("create queue");
        let observer = QueueObserver::open(&name);

        assert!(matches!(writer.write(FrameSource::Host(&[1; 5])), Err(Error::FrameSizeMismatch { expected: 6, actual: 5 })));
        // A rejected frame must not advance or publish queue state.
        assert_eq!((observer.write_idx(), observer.read_idx(), observer.state()), (0, 0, STATE_STARTING));

        for (sequence, value) in [(1, 10_u8), (2, 20), (3, 30), (4, 40)] {
            writer.write(FrameSource::Host(&[value; 6])).expect("publish frame");
            assert_eq!((observer.write_idx(), observer.read_idx(), observer.state()), (sequence, sequence, STATE_READY));
            let (timestamp, payload) = observer.slot(sequence, format.frame_size);
            assert!(timestamp > 0);
            assert_eq!(payload, [value; 6]);
        }
    }

    #[test]
    fn sequence_wraps_like_upstream_unsigned_index() {
        let name = test_name("Queue", "wrap");
        let mut writer = QueueWriter::create(&name, nv12(2, 2), ExistingQueue::Reject, CameraType::Obs).expect("create queue");
        let observer = QueueObserver::open(&name);
        QueueObserver::force_write_idx(&name, u32::MAX);
        writer.write(FrameSource::Host(&[9; 6])).expect("publish wrapped frame");
        assert_eq!((observer.write_idx(), observer.read_idx()), (0, 0));
        assert_eq!(observer.slot(0, 6).1, [9; 6]);
    }

    #[test]
    fn reinitialize_resets_a_retained_same_format_mapping() {
        let name = test_name("Queue", "reinitialize");
        let format = nv12(2, 2);
        let mut first = QueueWriter::create(&name, format, ExistingQueue::Reinitialize, CameraType::ManagedDirectShow).expect("first queue");
        let observer = QueueObserver::open(&name);
        first.write(FrameSource::Host(&[1; 6])).expect("first frame");
        drop(first);
        assert_eq!(observer.state(), STATE_STOPPING);

        // The observer still holds the section; a later producer reuses it.
        let _second = QueueWriter::create(&name, format, ExistingQueue::Reinitialize, CameraType::ManagedDirectShow).expect("reinitialize retained queue");
        assert_eq!((observer.write_idx(), observer.read_idx(), observer.state()), (0, 0, STATE_STARTING));
    }

    /// Phase 0A oracle: the 3-slot ABI has no per-slot generation, so a writer
    /// that laps a reader overwrites the slot the reader selected and the
    /// reader cannot tell from the slot alone. Protocol v2 work must replace
    /// this assertion rather than silently changing the current ABI.
    #[test]
    fn lapping_writer_reuses_the_selected_slot_without_a_generation_marker() {
        let name = test_name("Queue", "lap");
        let mut writer = QueueWriter::create(&name, nv12(2, 2), ExistingQueue::Reject, CameraType::Obs).expect("create queue");
        let observer = QueueObserver::open(&name);

        writer.write(FrameSource::Host(&[1; 6])).expect("frame 1");
        let selected = observer.read_idx();
        for value in [2_u8, 3, 4] {
            writer.write(FrameSource::Host(&[value; 6])).expect("lapping frame");
        }
        assert_eq!(observer.slot(selected, 6).1, [4; 6], "frame 4 reused frame 1's slot");
        assert_eq!(observer.read_idx().wrapping_sub(selected), SLOT_COUNT as u32);
    }

    /// Phase 0A torn-frame stress: a high-rate writer against a deliberately
    /// slow reader. Reports how often the current ABI delivers a torn frame.
    #[test]
    #[ignore = "Phase 0A stress measurement; run with --ignored --nocapture"]
    fn high_rate_writer_against_slow_reader_reports_torn_frames() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let name = test_name("Queue", "stress");
        let format = nv12(640, 480);
        let mut writer = QueueWriter::create(&name, format, ExistingQueue::Reject, CameraType::Obs).expect("create queue");
        let observer = QueueObserver::open(&name);
        let running = Arc::new(AtomicBool::new(true));
        let writer_running = Arc::clone(&running);

        let writer_thread = std::thread::spawn(move || {
            let mut frame = vec![0_u8; format.frame_size];
            let mut written = 0_u64;
            while writer_running.load(Ordering::Relaxed) {
                frame.fill(written as u8);
                writer.write(FrameSource::Host(&frame)).expect("stress write");
                written += 1;
            }
            written
        });

        let mut copy = vec![0_u8; format.frame_size];
        let (mut reads, mut torn) = (0_u64, 0_u64);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if observer.state() != STATE_READY {
                std::thread::yield_now();
                continue;
            }
            let (_, payload) = observer.slot(observer.read_idx(), format.frame_size);
            // Copy in small chunks with yields to widen the overwrite window.
            for (source, destination) in payload.chunks(4096).zip(copy.chunks_mut(4096)) {
                destination.copy_from_slice(source);
                std::thread::yield_now();
            }
            reads += 1;
            if copy.iter().any(|byte| *byte != copy[0]) {
                torn += 1;
            }
        }
        running.store(false, Ordering::Relaxed);
        let written = writer_thread.join().expect("writer thread");
        println!("frame-queue stress: written={written} reads={reads} torn={torn}");
        assert!(reads > 0 && written > reads);
    }
}
