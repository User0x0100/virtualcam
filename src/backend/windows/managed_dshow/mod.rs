//! Producer side of this crate's managed DirectShow camera (transport v2).
//!
//! DirectShow's control plane is producer-owned: each session takes the
//! single-writer lease, creates a fresh seqlock'd ring named by a random
//! generation, and publishes `{generation, format, pid}` in the fixed control
//! mapping. All objects live in the session-local namespace (see
//! `virtualcam_protocol::dshow`).
//!
//! Every published frame also sets the session's auto-reset frame event, so
//! the (single) consumer graph delivers it immediately instead of on its
//! own timer. The connected consumer's pixel format arrives through the
//! request mapping; acting on it is the caller's decision.

use virtualcam_protocol::dshow::{CONTROL_MAGIC, CONTROL_STATE_ACTIVE, CONTROL_STATE_STOPPED, CONTROL_VERSION, DshowControl, DshowNames, DshowRequest, REQUEST_STATE_CONNECTED};
use virtualcam_protocol::registry::values;
use virtualcam_protocol::ring::{DEFAULT_SLOT_COUNT, RingLayout, RingWriter};
use virtualcam_protocol::{ValidatedFormat, VideoFormat};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, SetEvent};

use super::FrameSource;
use crate::managed::metadata::history_path;
use crate::platform::registry::{Hive, Key};
use crate::platform::windows::{MappedView, OwnedHandle, QpcClock, current_process_start_time, opened_existing_object, random_nonzero_u64};
use crate::{CameraType, Error, ManagedCameraId, Result, WriteStatus};

/// A page-file-backed named mapping plus its whole-mapping view.
pub(super) struct NamedMapping {
    // Field order is drop order: unmap before closing the section.
    view: MappedView,
    _handle: OwnedHandle,
}

impl NamedMapping {
    /// Creates a new mapping; `Ok(None)` if the name already exists.
    pub(super) fn create_new(name: &[u16], size: usize, operation: &'static str) -> Result<Option<Self>> {
        Self::create(name, size, operation, false)
    }

    /// Creates a mapping or opens the existing one.
    pub(super) fn create_or_open(name: &[u16], size: usize, operation: &'static str) -> Result<Self> {
        Self::create(name, size, operation, true).map(|mapping| mapping.expect("existing mappings are accepted"))
    }

    fn create(name: &[u16], size: usize, operation: &'static str, accept_existing: bool) -> Result<Option<Self>> {
        let size = size as u64;
        // SAFETY: INVALID_HANDLE_VALUE selects the page file; `name` is
        // NUL-terminated and the returned handle is owned below.
        let raw = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null(), PAGE_READWRITE, (size >> 32) as u32, size as u32, name.as_ptr()) };
        // SAFETY: A non-null CreateFileMappingW result is one owned handle.
        let handle = unsafe { OwnedHandle::from_result(raw, operation) }?;
        if opened_existing_object() && !accept_existing {
            return Ok(None);
        }
        let view = MappedView::map(handle.raw(), FILE_MAP_ALL_ACCESS, operation)?;
        if view.accessible_len() < size as usize {
            return Err(Error::Protocol {
                backend: CameraType::ManagedDirectShow,
                detail: "shared mapping is smaller than required",
            });
        }
        Ok(Some(Self { view, _handle: handle }))
    }

    pub(super) fn view(&self) -> &MappedView {
        &self.view
    }
}

/// One producer session's ring and its frame event.
struct Session {
    // Field order is drop order: unmap the ring before closing the event.
    writer: RingWriter,
    _mapping: NamedMapping,
    frame_event: OwnedHandle,
}

pub(crate) struct ManagedDshowBackend {
    // Field order is drop order: ring, then control, then the lease last.
    ring: Option<Session>,
    control: Option<NamedMapping>,
    request: Option<NamedMapping>,
    record: DshowControl,
    /// Released on close, so a new session may open right away.
    lease: Option<OwnedHandle>,
    clock: QpcClock,
    history_id: Option<ManagedCameraId>,
    format: ValidatedFormat,
    first_frame_published: bool,
}

impl ManagedDshowBackend {
    pub(crate) fn open(id: ManagedCameraId, format: ValidatedFormat) -> Result<Self> {
        Self::open_named(&DshowNames::for_camera(id), format, Some(id))
    }

    /// `history_id` is `None` only in tests that must not write HKCU history.
    pub(super) fn open_named(names: &DshowNames, format: ValidatedFormat, history_id: Option<ManagedCameraId>) -> Result<Self> {
        let clock = QpcClock::new()?;
        // Only producers ever open the lease, so its existence is an exact
        // single-writer claim and consumers can never cause DeviceInUse.
        // SAFETY: NUL-terminated name, default security.
        let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, names.lease.as_ptr()) };
        // SAFETY: A non-null CreateEventW result is one owned handle.
        let lease = unsafe { OwnedHandle::from_result(raw, "CreateEventW(managed producer lease)") }?;
        if opened_existing_object() {
            return Err(Error::DeviceInUse);
        }

        let layout = RingLayout::for_format(&format, DEFAULT_SLOT_COUNT).map_err(|_| Error::FrameSizeOverflow)?;
        // SAFETY: Trivial process query.
        let pid = unsafe { GetCurrentProcessId() };
        let (generation, ring) = Self::create_ring(names, &format, layout, pid)?;

        let control = NamedMapping::create_or_open(&names.control, DshowControl::SIZE, "CreateFileMappingW(managed control)")?;
        // A consumer that connected first may already have created it.
        let request = NamedMapping::create_or_open(&names.request, DshowRequest::SIZE, "CreateFileMappingW(managed request)")?;
        let header = control.view().ptr_at::<DshowControl>(0).ok_or(Error::Protocol {
            backend: CameraType::ManagedDirectShow,
            detail: "control mapping is not aligned",
        })?;
        let record = DshowControl {
            magic: CONTROL_MAGIC,
            version: CONTROL_VERSION,
            state: CONTROL_STATE_ACTIVE,
            sequence: 0,
            generation,
            producer_pid: pid,
            slot_count: layout.slot_count,
            producer_start_time: current_process_start_time()?,
            ring_size: layout.total_size as u64,
            format: format.format().to_wire()?,
            reserved: [0; 6],
        };
        // SAFETY: The lease makes this the only writer of the aligned control view.
        unsafe { DshowControl::publish(header.as_ptr(), &record) };

        Ok(Self {
            ring: Some(ring),
            control: Some(control),
            request: Some(request),
            record,
            lease: Some(lease),
            clock,
            history_id,
            format,
            first_frame_published: false,
        })
    }

    fn create_ring(names: &DshowNames, format: &ValidatedFormat, layout: RingLayout, pid: u32) -> Result<(u64, Session)> {
        // A pre-existing object at a fresh random name is a collision or a
        // squatter; never reuse it, pick another generation.
        for _ in 0..4 {
            let generation = random_nonzero_u64()?;
            let Some(mapping) = NamedMapping::create_new(&names.ring(generation), layout.total_size, "CreateFileMappingW(managed ring)")? else {
                continue;
            };
            // SAFETY: NUL-terminated name, default security; auto-reset.
            let raw = unsafe { CreateEventW(core::ptr::null(), 0, 0, names.frame_event(generation).as_ptr()) };
            // SAFETY: A non-null CreateEventW result is one owned handle.
            let frame_event = unsafe { OwnedHandle::from_result(raw, "CreateEventW(managed frame event)") }?;
            if opened_existing_object() {
                continue;
            }
            // SAFETY: The freshly created view is exclusively ours and stays
            // mapped as long as the writer (they are stored together).
            let writer = unsafe { RingWriter::initialize(mapping.view().as_ptr(), mapping.view().accessible_len(), format, layout.slot_count, generation, pid) }.map_err(|_| Error::Protocol {
                backend: CameraType::ManagedDirectShow,
                detail: "could not initialize the frame ring",
            })?;
            return Ok((generation, Session { writer, _mapping: mapping, frame_event }));
        }
        Err(Error::Protocol {
            backend: CameraType::ManagedDirectShow,
            detail: "could not create a unique frame ring",
        })
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        // No consumer-presence handshake: an idle consumer shows black and a
        // connecting one picks up the live format, so a session is ready.
        self.ring.as_ref().map(|_| true).ok_or(Error::AlreadyClosed)
    }

    /// The format the connected consumer graph negotiated, if any.
    pub(crate) fn requested_format(&self) -> Option<VideoFormat> {
        let header = self.request.as_ref()?.view().ptr_at::<DshowRequest>(0)?;
        // SAFETY: Live, aligned request view of sufficient size.
        let record = unsafe { DshowRequest::snapshot(header.as_ptr()) }?;
        if record.state != REQUEST_STATE_CONNECTED {
            return None;
        }
        record.format.decode().ok().filter(|format| format.validate().is_ok())
    }

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        let Session { writer, frame_event, .. } = self.ring.as_mut().ok_or(Error::AlreadyClosed)?;
        if frame.byte_len() != writer.frame_size() {
            return Err(Error::FrameSizeMismatch {
                expected: writer.frame_size(),
                actual: frame.byte_len(),
            });
        }
        let payload = writer.begin_frame();
        if let Err(error) = frame.copy_to(payload) {
            writer.abort_frame();
            return Err(error);
        }
        writer.commit_frame(self.clock.now_100ns()?);
        // SAFETY: Live event handle owned by this session.
        unsafe { SetEvent(frame_event.raw()) };
        if !self.first_frame_published {
            self.first_frame_published = true;
            self.record_history();
        }
        Ok(WriteStatus::Written)
    }

    /// Once per session, after the first frame, remember this format so an
    /// idle consumer opened later advertises it.
    fn record_history(&self) {
        let Some(id) = self.history_id else { return };
        let result = self
            .format
            .format()
            .to_wire()
            .map_err(Error::from)
            .and_then(|wire| Key::create(Hive::CurrentUser, &history_path(id))?.set_binary(values::LAST_FORMAT, &wire.to_bytes()));
        if let Err(error) = result {
            trace_warn!(%error, %id, "could not persist managed camera's last format");
            #[cfg(not(feature = "tracing"))]
            let _ = error;
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if let Some(session) = self.ring.as_mut() {
            session.writer.stop();
            // Wake the consumer so it notices the stop without waiting.
            // SAFETY: Live event handle owned by this session.
            unsafe { SetEvent(session.frame_event.raw()) };
        }
        if let Some(control) = &self.control
            && let Some(header) = control.view().ptr_at::<DshowControl>(0)
        {
            let stopped = DshowControl {
                state: CONTROL_STATE_STOPPED,
                ..self.record
            };
            // SAFETY: Still the lease holder; aligned control view.
            unsafe { DshowControl::publish(header.as_ptr(), &stopped) };
        }
        self.ring = None;
        self.control = None;
        self.request = None;
        self.lease = None;
        Ok(())
    }
}

impl Drop for ManagedDshowBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Decodes the last format a producer published for `id` (DirectShow idle mode).
#[cfg(test)]
pub(crate) fn last_format(id: ManagedCameraId) -> Option<virtualcam_protocol::VideoFormat> {
    use virtualcam_protocol::WireVideoFormat;
    let key = Key::open(Hive::CurrentUser, &history_path(id)).ok()??;
    let bytes = key.get_binary(values::LAST_FORMAT).ok()??;
    WireVideoFormat::from_bytes(&bytes).and_then(WireVideoFormat::decode).ok()
}

#[cfg(test)]
mod tests {
    use virtualcam_protocol::dshow::CONTROL_STATE_STOPPED;
    use virtualcam_protocol::ring::{RING_STATE_ACTIVE, RING_STATE_STOPPED, ReadOutcome, RingReader};
    use virtualcam_protocol::{FrameRate, PixelFormat, VideoFormat};
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};

    use super::*;

    fn names(suffix: &str) -> DshowNames {
        DshowNames::with_base(format!("Local\\VirtualCamRustManaged2_{}_{suffix}", std::process::id()))
    }

    fn format(pixel_format: PixelFormat, width: u32, height: u32) -> ValidatedFormat {
        VideoFormat::new(pixel_format, width, height, FrameRate::fps(30)).validate().expect("valid")
    }

    fn open_view(name: &[u16]) -> (MappedView, OwnedHandle) {
        // SAFETY: NUL-terminated test name; the result is owned below.
        let raw = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        // SAFETY: A non-null OpenFileMappingW result is one owned handle.
        let handle = unsafe { OwnedHandle::from_result(raw, "OpenFileMappingW(test)") }.expect("mapping exists");
        (MappedView::map(handle.raw(), FILE_MAP_READ, "MapViewOfFile(test)").expect("map"), handle)
    }

    fn control(view: &MappedView) -> DshowControl {
        // SAFETY: The view maps a live control record.
        unsafe { DshowControl::snapshot(view.as_ptr().cast()) }.expect("consistent control")
    }

    #[test]
    fn lease_rejects_a_second_producer_until_close() {
        let names = names("lease");
        let mut first = ManagedDshowBackend::open_named(&names, format(PixelFormat::Nv12, 64, 64), None).expect("first producer");
        assert!(matches!(ManagedDshowBackend::open_named(&names, format(PixelFormat::Nv12, 64, 64), None), Err(Error::DeviceInUse)));
        first.close().expect("close");
        drop(first);
        let mut second = ManagedDshowBackend::open_named(&names, format(PixelFormat::Yuy2, 64, 64), None).expect("reopen after close and drop");
        // Closing alone releases the lease; the closed object may still be alive.
        second.close().expect("close");
        let _third = ManagedDshowBackend::open_named(&names, format(PixelFormat::Nv12, 32, 32), None).expect("reopen after close only");
        drop(second);
    }

    #[test]
    fn session_publishes_control_and_ring_then_stops() {
        let names = names("lifecycle");
        let mut producer = ManagedDshowBackend::open_named(&names, format(PixelFormat::Uyvy, 4, 2), None).expect("open");
        let (control_view, _control) = open_view(&names.control);
        let record = control(&control_view);
        assert_eq!(record.state, CONTROL_STATE_ACTIVE);
        assert_eq!(record.format.decode().expect("format"), format(PixelFormat::Uyvy, 4, 2).format());
        assert_eq!(record.producer_start_time, current_process_start_time().expect("start time"));

        let (ring_view, _ring) = open_view(&names.ring(record.generation));
        // SAFETY: The view stays mapped for the reader's lifetime.
        let reader = unsafe { RingReader::open(ring_view.as_ptr(), ring_view.accessible_len()) }.expect("ring");
        assert_eq!(reader.generation(), record.generation);

        let frame: Vec<u8> = (0..16).collect();
        assert_eq!(producer.write_frame(FrameSource::Host(&frame)), Ok(WriteStatus::Written));
        assert_eq!(reader.state(), RING_STATE_ACTIVE);
        let mut out = [0_u8; 16];
        let outcome = reader.read_latest(|src| {
            // SAFETY: 16-byte payload.
            unsafe { core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), 16) }
        });
        assert!(matches!(outcome, ReadOutcome::Frame { sequence: 1, .. }));
        assert_eq!(out.as_slice(), frame.as_slice());
        assert!(matches!(producer.write_frame(FrameSource::Host(&frame[..8])), Err(Error::FrameSizeMismatch { .. })));

        producer.close().expect("close");
        producer.close().expect("idempotent");
        assert_eq!(control(&control_view).state, CONTROL_STATE_STOPPED);
        assert_eq!(reader.state(), RING_STATE_STOPPED);
        assert_eq!(producer.write_frame(FrameSource::Host(&frame)), Err(Error::AlreadyClosed));
    }

    #[test]
    fn each_session_gets_a_fresh_ring_beside_retained_old_ones() {
        let names = names("generations");
        let mut first = ManagedDshowBackend::open_named(&names, format(PixelFormat::Nv12, 64, 64), None).expect("first");
        let (control_view, _control) = open_view(&names.control);
        let old_generation = control(&control_view).generation;
        // A consumer keeps the old ring mapped across the producer restart.
        let _held = open_view(&names.ring(old_generation));
        first.close().expect("close");
        drop(first);

        let _second = ManagedDshowBackend::open_named(&names, format(PixelFormat::P010, 1920, 1080), None).expect("larger format");
        let record = control(&control_view);
        assert_ne!(record.generation, old_generation);
        assert_eq!(record.format.decode().expect("format").pixel_format, PixelFormat::P010);
    }

    #[test]
    fn first_frame_records_per_user_history() {
        let id = ManagedCameraId::from_u128(0x5EED0000_0000_4000_8000_000000000000 | u128::from(std::process::id()));
        let _cleanup = HistoryCleanup(id);
        let names = names("history");
        let format = format(PixelFormat::Bgra32, 2, 2);
        let mut producer = ManagedDshowBackend::open_named(&names, format, Some(id)).expect("open");
        assert_eq!(last_format(id), None);
        producer.write_frame(FrameSource::Host(&[0; 16])).expect("write");
        assert_eq!(last_format(id), Some(format.format()));
    }

    struct HistoryCleanup(ManagedCameraId);

    impl Drop for HistoryCleanup {
        fn drop(&mut self) {
            let _ = Key::delete_tree(Hive::CurrentUser, &history_path(self.0));
        }
    }
}
