//! Producer side of this crate's managed DirectShow camera.
//!
//! DirectShow's control plane is producer-owned: each session chooses its NV12
//! mode, reserves the single-writer lease, declares the mode in a fixed control
//! mapping, and writes a format-specific OBS-compatible frame queue. All objects
//! live in the session-local `Local\VirtualCamRs-{CLSID}` namespace.

mod history;

use core::ptr::addr_of_mut;

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use super::FrameSource;
use super::protocol::dshow_control::{CONTROL_STATE_READY, CONTROL_STATE_STARTING, CONTROL_STATE_STOPPING, DshowControlHeader, DshowObjectNames};
use super::protocol::publication;
use super::queue_writer::{ExistingQueue, QueueWriter};
use crate::config::Nv12Format;
use crate::platform::windows::{MappedView, OwnedHandle, last_error, opened_existing_object};
use crate::{CameraType, Error, ManagedCameraId, Result, WriteStatus};

pub(crate) struct ManagedDshowBackend {
    // Field order is drop order: the data queue unmaps before the session
    // releases the control plane and, last, the producer claim.
    queue: Option<QueueWriter>,
    session: Option<ProducerSession>,
    history_id: Option<ManagedCameraId>,
    format: Nv12Format,
    first_frame_published: bool,
}

impl ManagedDshowBackend {
    pub(crate) fn open(id: ManagedCameraId, format: Nv12Format) -> Result<Self> {
        Self::open_named(&DshowObjectNames::for_camera(id), format, Some(id))
    }

    /// `history_id` is `None` only in tests that must not write HKCU history.
    fn open_named(names: &DshowObjectNames, format: Nv12Format, history_id: Option<ManagedCameraId>) -> Result<Self> {
        let session = ProducerSession::reserve(names)?;
        // The producer claim is the single-writer lease, so a retained
        // same-format data section can be safely reinitialized, and different
        // formats never share section capacity.
        let queue = QueueWriter::create(&names.data(format), format, ExistingQueue::Reinitialize, CameraType::ManagedDirectShow)?;
        session.publish(format)?;
        Ok(Self {
            queue: Some(queue),
            session: Some(session),
            history_id,
            format,
            first_frame_published: false,
        })
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        // Like OBS there is no consumer-presence handshake; a live session is
        // ready, and the DirectShow source emits black until frames arrive.
        self.queue.as_ref().map(|_| true).ok_or(Error::AlreadyClosed)
    }

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        self.queue.as_mut().ok_or(Error::AlreadyClosed)?.write(frame)?;
        if !self.first_frame_published {
            self.first_frame_published = true;
            if let Some(session) = &self.session {
                session.set_state(CONTROL_STATE_READY);
            }
            self.record_history();
        }
        Ok(WriteStatus::Written)
    }

    /// Once per session, after the first successful frame, remember this mode
    /// so an idle consumer opened later advertises it.
    fn record_history(&self) {
        if let Some(id) = self.history_id
            && let Err(error) = history::persist(id, self.format)
        {
            trace_warn!(error, %id, "could not persist managed camera's last successful output format");
            #[cfg(not(feature = "tracing"))]
            let _ = error;
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        // Publish data and control STOPPING before releasing any transport
        // handle; the producer claim is released last.
        if let Some(queue) = self.queue.as_mut() {
            queue.stop();
        }
        if let Some(session) = self.session.as_mut() {
            session.stop();
        }
        self.queue = None;
        self.session = None;
        Ok(())
    }
}

impl Drop for ManagedDshowBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// The producer-owned lease, Ready event, and fixed control mapping of one session.
struct ProducerSession {
    // Field order is drop order; the claim must be released last.
    control: MappedView,
    _control_mapping: OwnedHandle,
    ready: OwnedHandle,
    claim: OwnedHandle,
    stopped: bool,
}

impl ProducerSession {
    /// Reserves the camera for this producer and invalidates stale control data.
    fn reserve(names: &DshowObjectNames) -> Result<Self> {
        // Only producers open this named object, so its lifetime is an exact
        // single-writer lease. It remains unsignaled until the complete
        // session has been published.
        // SAFETY: Stable NUL-terminated name; null security attributes.
        let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, names.producer.as_ptr()) };
        // SAFETY: A non-null CreateEventW result is one owned handle.
        let claim = unsafe { OwnedHandle::from_result(raw, "CreateEventW(managed producer claim)") }?;
        if opened_existing_object() {
            return Err(Error::DeviceInUse);
        }

        // Ready is a reusable notification object, not an ownership token. A
        // DirectShow consumer may retain it across producer sessions.
        // SAFETY: Stable NUL-terminated name; null security attributes.
        let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, names.ready.as_ptr()) };
        // SAFETY: A non-null CreateEventW result is one owned handle.
        let ready = unsafe { OwnedHandle::from_result(raw, "CreateEventW(managed runtime ready)") }?;
        // Clear a stale signaled state before invalidating/publishing the new
        // control header. The producer claim is still unsignaled.
        // SAFETY: `ready` is a live manual-reset event with modify-state access.
        if unsafe { ResetEvent(ready.raw()) } == 0 {
            return Err(last_error("ResetEvent(managed runtime ready)"));
        }

        const CONTROL_SIZE: u32 = core::mem::size_of::<DshowControlHeader>() as u32;
        // SAFETY: Page-file-backed fixed-size control mapping with a stable name.
        let raw = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null(), PAGE_READWRITE, 0, CONTROL_SIZE, names.control.as_ptr()) };
        // SAFETY: A non-null CreateFileMappingW result is one owned handle.
        let control_mapping = unsafe { OwnedHandle::from_result(raw, "CreateFileMappingW(managed control)") }?;
        let control = MappedView::map(control_mapping.raw(), FILE_MAP_ALL_ACCESS, "MapViewOfFile(managed control)")?;
        let header = control.ptr_at::<DshowControlHeader>(0).ok_or(Error::Protocol {
            backend: CameraType::ManagedDirectShow,
            detail: "managed control mapping does not provide an aligned control header",
        })?;
        // Invalidate stale session metadata immediately. No consumer treats the
        // new session as live until Ready and the producer claim are signaled.
        // SAFETY: `header` is aligned writable storage inside the mapped view.
        unsafe {
            publication::publish_u32(addr_of_mut!((*header.as_ptr()).state), 0);
            core::ptr::write_bytes(header.as_ptr(), 0, 1);
        }

        Ok(Self {
            control,
            _control_mapping: control_mapping,
            ready,
            claim,
            stopped: false,
        })
    }

    /// Declares `format`, then signals Ready and finally the producer claim.
    ///
    /// Called only after the data queue and all other fallible state exist, so
    /// a signaled claim always describes a complete session.
    fn publish(&self, format: Nv12Format) -> Result<()> {
        let header = self.header();
        let declaration = DshowControlHeader {
            state: 0,
            ..DshowControlHeader::starting(format)
        };
        // SAFETY: `reserve` validated this aligned writable header.
        unsafe {
            core::ptr::copy_nonoverlapping(&raw const declaration, header, 1);
            publication::publish_u32(addr_of_mut!((*header).state), CONTROL_STATE_STARTING);
        }
        // SAFETY: Both are live manual-reset events owned by this session.
        if unsafe { SetEvent(self.ready.raw()) } == 0 {
            return Err(last_error("SetEvent(managed runtime ready)"));
        }
        // Existence reserves ownership; signaling the claim is the final commit
        // point that tells native consumers the session is complete.
        // SAFETY: As above.
        if unsafe { SetEvent(self.claim.raw()) } == 0 {
            return Err(last_error("SetEvent(managed producer claim)"));
        }
        Ok(())
    }

    fn set_state(&self, state: u32) {
        // SAFETY: The control view stays mapped for this session's lifetime.
        unsafe { publication::publish_u32(addr_of_mut!((*self.header()).state), state) };
    }

    fn stop(&mut self) {
        if !self.stopped {
            self.stopped = true;
            self.set_state(CONTROL_STATE_STOPPING);
        }
    }

    fn header(&self) -> *mut DshowControlHeader {
        self.control.as_ptr().cast()
    }
}

impl Drop for ProducerSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use core::ptr::addr_of;

    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};
    use windows_sys::Win32::System::Threading::{OpenEventW, WaitForSingleObject};

    use super::super::protocol::dshow_control::{CONTROL_MAGIC, CONTROL_VERSION};
    use super::super::protocol::frame_queue::{STATE_READY, STATE_STOPPING};
    use super::super::queue_writer::tests::{QueueObserver, nv12};
    use super::*;

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

    fn names(suffix: &str) -> DshowObjectNames {
        DshowObjectNames::with_base(format!("Local\\VirtualCamRustManaged_{}_{suffix}", std::process::id()))
    }

    fn open_event(name: &[u16]) -> OwnedHandle {
        // SAFETY: Stable NUL-terminated test-only kernel object name.
        let raw = unsafe { OpenEventW(SYNCHRONIZE_ACCESS, 0, name.as_ptr()) };
        // SAFETY: A non-null OpenEventW result is one owned handle.
        unsafe { OwnedHandle::from_result(raw, "OpenEventW(test)") }.expect("event exists")
    }

    fn signaled(event: &OwnedHandle) -> bool {
        // SAFETY: `event` is a live SYNCHRONIZE handle.
        unsafe { WaitForSingleObject(event.raw(), 0) == WAIT_OBJECT_0 }
    }

    fn control(names: &DshowObjectNames) -> (MappedView, OwnedHandle) {
        // SAFETY: Stable NUL-terminated test-only mapping name.
        let raw = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, names.control.as_ptr()) };
        // SAFETY: A non-null OpenFileMappingW result is one owned handle.
        let mapping = unsafe { OwnedHandle::from_result(raw, "OpenFileMappingW(test control)") }.expect("control exists");
        (MappedView::map(mapping.raw(), FILE_MAP_READ, "MapViewOfFile(test control)").expect("map control"), mapping)
    }

    fn control_header(view: &MappedView) -> DshowControlHeader {
        let header = view.as_ptr().cast::<DshowControlHeader>();
        // SAFETY: The view maps a live aligned control header.
        unsafe {
            let state = publication::observe_u32(addr_of!((*header).state));
            DshowControlHeader { state, ..header.read() }
        }
    }

    #[test]
    fn producer_claim_reserves_writer_before_mapping_exists() {
        let names = names("reservation");
        // SAFETY: unique NUL-terminated test name; the returned handle is owned.
        let raw = unsafe { CreateEventW(core::ptr::null(), 1, 0, names.producer.as_ptr()) };
        // SAFETY: A non-null CreateEventW result is one owned handle.
        let _claim = unsafe { OwnedHandle::from_result(raw, "CreateEventW(test claim)") }.expect("create claim");

        assert!(matches!(ManagedDshowBackend::open_named(&names, nv12(64, 64), None), Err(Error::DeviceInUse)));
    }

    #[test]
    fn session_publishes_control_then_ready_then_claim_and_stops_on_close() {
        let names = names("lifecycle");
        let format = nv12(4, 2);
        let mut producer = ManagedDshowBackend::open_named(&names, format, None).expect("open managed producer");
        let claim = open_event(&names.producer);
        let ready = open_event(&names.ready);
        assert!(signaled(&claim) && signaled(&ready));

        let (view, _mapping) = control(&names);
        let declared = control_header(&view);
        assert_eq!((declared.magic, declared.version, declared.state), (CONTROL_MAGIC, CONTROL_VERSION, CONTROL_STATE_STARTING));
        assert_eq!((declared.width, declared.height, declared.interval_100ns), (4, 2, 333_333));

        let data = QueueObserver::open(&names.data(format));
        assert_eq!(producer.is_ready(), Ok(true));
        assert_eq!(producer.write_frame(FrameSource::Host(&[16; 12])), Ok(WriteStatus::Written));
        assert_eq!(control_header(&view).state, CONTROL_STATE_READY);
        assert_eq!((data.read_idx(), data.state()), (1, STATE_READY));

        producer.close().expect("close managed producer");
        producer.close().expect("close is idempotent");
        assert_eq!(producer.write_frame(FrameSource::Host(&[16; 12])), Err(Error::AlreadyClosed));
        assert_eq!(control_header(&view).state, CONTROL_STATE_STOPPING);
        assert_eq!(data.state(), STATE_STOPPING);
    }

    #[test]
    fn reopen_allows_consumer_held_objects_and_larger_next_format() {
        let names = names("consumer_holds_objects");
        let first_format = nv12(64, 64);
        let mut first = ManagedDshowBackend::open_named(&names, first_format, None).expect("first managed producer opens");

        // Model a DirectShow consumer that retains the old transport objects.
        // The next producer must not be constrained by this old data section's
        // capacity because data mappings are format-specific in managed ABI v3.
        let held_ready = open_event(&names.ready);
        let _held_control = control(&names);
        let _held_old_data = QueueObserver::open(&names.data(first_format));

        assert!(matches!(ManagedDshowBackend::open_named(&names, first_format, None), Err(Error::DeviceInUse)));

        first.close().expect("first managed producer closes");
        let mut second = ManagedDshowBackend::open_named(&names, nv12(1920, 1080), None).expect("larger producer opens beside retained old data section");
        // The reopened producer reset and then re-signaled the reusable Ready event.
        assert!(signaled(&held_ready));
        second.close().expect("second managed producer closes");
    }
}
