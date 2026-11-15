//! Producer side of this crate's managed Media Foundation camera.
//!
//! The Frame Server source owns the declared format set and the transport. A
//! producer attaches lazily: at most every 250 ms it tries the camera's
//! rendezvous pipe (absent until a client opens the camera), authenticates by
//! its token, and maps the source-created ring for writing. Until then frames
//! are dropped with [`WriteStatus::ConsumerInactive`]. The open pipe is the
//! liveness link in both directions, and carries the source's `Request`s:
//! the mode consumers selected, exposed through `requested_format` so the
//! application can reopen in that mode (any declared mode). The session's
//! format is the ring's format; the source copies it only while consumers
//! have selected the same mode and shows black otherwise.

use std::time::{Duration, Instant};

use virtualcam_protocol::mf::{Hello, Request, Welcome, WelcomeStatus, pipe_name};
use virtualcam_protocol::ring::RingWriter;
use virtualcam_protocol::{CameraId, ValidatedFormat, VideoFormat};
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAG_OVERLAPPED, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, OpenFileMappingW};
use windows_sys::Win32::System::Pipes::{PIPE_READMODE_MESSAGE, PeekNamedPipe, SetNamedPipeHandleState};
use windows_sys::Win32::System::Threading::{CreateEventW, EVENT_MODIFY_STATE, OpenEventW, WaitForSingleObject};

use super::FrameSource;
use crate::platform::windows::{MappedView, OwnedHandle, QpcClock, wide_nul};
use crate::{CameraType, Error, Result, WriteStatus};

const ATTACH_RETRY: Duration = Duration::from_millis(250);
const LIVENESS_PERIOD: Duration = Duration::from_millis(100);
const HANDSHAKE_TIMEOUT_MS: u32 = 2_000;
/// `FILE_READ_DATA | FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE`:
/// exactly what the source's pipe DACL grants (no create-instance right).
const PIPE_ACCESS: u32 = 0x0001 | 0x0002 | FILE_WRITE_ATTRIBUTES | 0x0010_0000;

struct Session {
    // Field order is drop order: writer, view, section, then the pipe whose
    // closure tells the source to detach.
    writer: RingWriter,
    _view: MappedView,
    _section: OwnedHandle,
    pipe: OwnedHandle,
    frame_event: OwnedHandle,
    next_liveness_check: Instant,
    /// The mode the source last asked for.
    requested: Option<VideoFormat>,
}

pub(crate) struct ManagedMfBackend {
    camera_id: CameraId,
    format: ValidatedFormat,
    pipe_name: Vec<u16>,
    session: Option<Session>,
    next_attempt: Instant,
    clock: QpcClock,
    closed: bool,
}

/// Completes one overlapped pipe operation with a deadline.
fn overlapped_io(pipe: HANDLE, start: impl FnOnce(*mut OVERLAPPED) -> i32) -> Option<u32> {
    // SAFETY: Anonymous manual-reset event; owned below.
    let event = unsafe { OwnedHandle::from_result(CreateEventW(core::ptr::null(), 1, 0, core::ptr::null()), "CreateEventW(pipe io)") }.ok()?;
    // SAFETY: OVERLAPPED is plain data; zero is its documented initial state.
    let mut ov: OVERLAPPED = unsafe { core::mem::zeroed() };
    ov.hEvent = event.raw();
    // SAFETY: GetLastError reads thread state right after `start`.
    if start(&mut ov) == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
        return None;
    }
    let mut bytes = 0;
    // SAFETY: Live event handle.
    if unsafe { WaitForSingleObject(event.raw(), HANDSHAKE_TIMEOUT_MS) } != WAIT_OBJECT_0 {
        // SAFETY: Cancel and drain so `ov` may be dropped.
        unsafe {
            CancelIoEx(pipe, &ov);
            GetOverlappedResult(pipe, &ov, &mut bytes, 1);
        }
        return None;
    }
    // SAFETY: The operation completed.
    (unsafe { GetOverlappedResult(pipe, &ov, &mut bytes, 0) } != 0).then_some(bytes)
}

impl ManagedMfBackend {
    pub(crate) fn open(camera_id: CameraId, format: ValidatedFormat, rendezvous_id: &str) -> Result<Self> {
        let mut backend = Self {
            camera_id,
            format,
            pipe_name: pipe_name(rendezvous_id),
            session: None,
            next_attempt: Instant::now(),
            clock: QpcClock::new()?,
            closed: false,
        };
        backend.attach_if_due()?;
        Ok(backend)
    }

    /// Tries the rendezvous when the retry period elapsed. Permanent
    /// rejections (identity, format, version) are errors; a missing or busy
    /// source is not.
    fn attach_if_due(&mut self) -> Result<()> {
        if self.session.is_some() || Instant::now() < self.next_attempt {
            return Ok(());
        }
        self.next_attempt = Instant::now() + ATTACH_RETRY;
        self.session = self.attach()?;
        Ok(())
    }

    fn attach(&self) -> Result<Option<Session>> {
        // Identification level only: the source may learn who we are but
        // can never act as us.
        // SAFETY: NUL-terminated pipe path; the handle is owned below.
        let raw = unsafe {
            CreateFileW(
                self.pipe_name.as_ptr(),
                PIPE_ACCESS,
                0,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                core::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Ok(None);
        }
        // SAFETY: CreateFileW returned one owned handle.
        let pipe = unsafe { OwnedHandle::from_raw(raw) };
        let mode = PIPE_READMODE_MESSAGE;
        // SAFETY: Live pipe handle and mode value.
        if unsafe { SetNamedPipeHandleState(pipe.raw(), &mode, core::ptr::null(), core::ptr::null()) } == 0 {
            return Ok(None);
        }
        let hello = Hello {
            camera_id: self.camera_id,
            format: self.format.format(),
        }
        .encode()
        .map_err(|_| Error::UnsupportedFormat)?;
        // SAFETY: `hello` outlives the drained overlapped write.
        if overlapped_io(pipe.raw(), |ov| unsafe { WriteFile(pipe.raw(), hello.as_ptr(), hello.len() as u32, core::ptr::null_mut(), ov) }) != Some(Hello::SIZE as u32) {
            return Ok(None);
        }
        let mut reply = [0_u8; Welcome::SIZE];
        // SAFETY: `reply` outlives the drained overlapped read.
        let Some(read) = overlapped_io(pipe.raw(), |ov| unsafe { ReadFile(pipe.raw(), reply.as_mut_ptr(), reply.len() as u32, core::ptr::null_mut(), ov) }) else {
            return Ok(None);
        };
        let welcome = Welcome::decode(&reply[..read as usize]).map_err(|_| Error::Protocol {
            backend: CameraType::ManagedMediaFoundation,
            detail: "malformed rendezvous response",
        })?;
        match welcome.status {
            WelcomeStatus::Ok => {}
            WelcomeStatus::Busy | WelcomeStatus::Internal => return Ok(None),
            WelcomeStatus::FormatMismatch => return Err(Error::FormatMismatch),
            WelcomeStatus::CameraMismatch | WelcomeStatus::Version => {
                return Err(Error::Protocol {
                    backend: CameraType::ManagedMediaFoundation,
                    detail: "Media Foundation source rejected the camera or protocol version",
                });
            }
            WelcomeStatus::Unauthorized => {
                return Err(Error::Protocol {
                    backend: CameraType::ManagedMediaFoundation,
                    detail: "Media Foundation source rejected this user as the camera owner",
                });
            }
        }
        let name = wide_nul(&welcome.ring_name);
        // SAFETY: NUL-terminated name from the authenticated source.
        let raw = unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, name.as_ptr()) };
        // SAFETY: A non-null result is one owned handle.
        let section = unsafe { OwnedHandle::from_result(raw, "OpenFileMappingW(MF ring)") }?;
        let view = MappedView::map(section.raw(), FILE_MAP_READ | FILE_MAP_WRITE, "MapViewOfFile(MF ring)")?;
        // SAFETY: The view stays mapped as long as the writer (stored together).
        let writer = unsafe { RingWriter::attach(view.as_ptr(), view.accessible_len(), &self.format) }.map_err(|_| Error::Protocol {
            backend: CameraType::ManagedMediaFoundation,
            detail: "Media Foundation ring does not match the native format",
        })?;
        let event_name = wide_nul(&welcome.frame_event_name);
        // SAFETY: Source-owned event, producer needs only EVENT_MODIFY_STATE.
        let frame_event = unsafe { OwnedHandle::from_result(OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()), "OpenEventW(MF frame)") }?;
        Ok(Some(Session {
            writer,
            _view: view,
            _section: section,
            pipe,
            frame_event,
            next_liveness_check: Instant::now() + LIVENESS_PERIOD,
            requested: None,
        }))
    }

    /// Drops the session when the source closed the pipe, and reads the
    /// source's pending requests.
    fn check_liveness(&mut self) {
        let Some(session) = self.session.as_mut() else { return };
        let now = Instant::now();
        if now < session.next_liveness_check {
            return;
        }
        session.next_liveness_check = now + LIVENESS_PERIOD;
        loop {
            let mut available = 0_u32;
            // SAFETY: Live pipe handle; non-blocking query.
            if unsafe { PeekNamedPipe(session.pipe.raw(), core::ptr::null_mut(), 0, core::ptr::null_mut(), &mut available, core::ptr::null_mut()) } == 0 {
                self.session = None;
                return;
            }
            if available == 0 {
                return;
            }
            let pipe = session.pipe.raw();
            let mut message = [0_u8; Request::SIZE];
            // SAFETY: `message` outlives the drained overlapped read.
            let read = overlapped_io(pipe, |ov| unsafe { ReadFile(pipe, message.as_mut_ptr(), message.len() as u32, core::ptr::null_mut(), ov) });
            match read.map(|read| Request::decode(&message[..read as usize])) {
                Some(Ok(request)) => session.requested = Some(request.format),
                // A malformed or oversized message: the source is not one we
                // understand; drop the session (it retries later).
                _ => {
                    self.session = None;
                    return;
                }
            }
        }
    }

    /// The mode the source asked for, when it differs from the one written.
    pub(crate) fn requested_format(&self) -> Option<VideoFormat> {
        virtualcam_protocol::delivery::requested_format(self.format.format(), self.session.as_ref().and_then(|session| session.requested))
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        if self.closed {
            return Err(Error::AlreadyClosed);
        }
        self.check_liveness();
        self.attach_if_due()?;
        Ok(self.session.is_some())
    }

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        if self.closed {
            return Err(Error::AlreadyClosed);
        }
        if frame.byte_len() != self.format.frame_size() {
            return Err(Error::FrameSizeMismatch {
                expected: self.format.frame_size(),
                actual: frame.byte_len(),
            });
        }
        self.check_liveness();
        self.attach_if_due()?;
        let timestamp = self.clock.now_100ns()?;
        let Some(session) = self.session.as_mut() else {
            return Ok(WriteStatus::ConsumerInactive);
        };
        super::managed_frame::write_frame(&mut session.writer, &session.frame_event, frame, timestamp)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        self.closed = true;
        self.session = None;
        Ok(())
    }
}

impl Drop for ManagedMfBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
