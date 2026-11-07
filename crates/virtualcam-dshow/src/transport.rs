//! Consumer side of the DirectShow transport v2.
//!
//! Per delivered frame the hot path is: one control snapshot (plain shared
//! memory reads), one seqlock'd ring read straight into the sample buffer,
//! and at most one zero-timeout wait on the producer process every 200 ms.
//! Kernel objects are opened only when a producer session (generation)
//! changes, and missing objects are re-probed at most twice per second.
//!
//! Each session also exposes the producer's auto-reset frame event, which
//! the push thread waits on to deliver every frame as soon as it exists.

use std::sync::Arc;

use virtualcam_protocol::delivery::FrameId;
use virtualcam_protocol::dshow::{CONTROL_STATE_ACTIVE, DshowControl, DshowNames};
use virtualcam_protocol::planes::copy_frame;
use virtualcam_protocol::ring::{RING_STATE_STOPPED, ReadOutcome, RingReader};
use virtualcam_protocol::{CameraId, FrameLayout, VideoFormat};
use windows::Win32::Foundation::{FILETIME, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{GetProcessTimes, OpenEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};
use windows_core::PCWSTR;

use crate::win::{Handle, ReadView, now_100ns};

const CONTROL_RETRY_100NS: u64 = 5_000_000;
const SESSION_RETRY_100NS: u64 = 5_000_000;
const LIVENESS_PERIOD_100NS: u64 = 2_000_000;

struct Session {
    generation: u64,
    format: VideoFormat,
    source_layout: FrameLayout,
    // Field order is drop order: the reader's view outlives the reader.
    reader: RingReader,
    _view: ReadView,
    producer: Option<Arc<Handle>>,
    /// `None` for producers that predate frame events: timer pacing.
    frame_event: Option<Arc<Handle>>,
    next_liveness_check: u64,
    alive: bool,
}

impl Session {
    fn serves(&self, want: &VideoFormat) -> bool {
        self.alive && self.format.pixel_format == want.pixel_format && self.format.width == want.width && self.format.height == want.height
    }
}

pub(crate) struct LiveSignal {
    pub(crate) event: Arc<Handle>,
    pub(crate) frame: FrameId,
    pub(crate) producer: Option<Arc<Handle>>,
}

/// Lazily attached view of one camera's producer sessions.
pub(crate) struct Transport {
    names: DshowNames,
    control: Option<ReadView>,
    next_control_attempt: u64,
    session: Option<Session>,
    failed_generation: Option<(u64, u64)>,
}

impl Transport {
    pub(crate) fn new(id: CameraId) -> Self {
        Self::with_names(DshowNames::for_camera(id))
    }

    pub(crate) fn with_names(names: DshowNames) -> Self {
        Self {
            names,
            control: None,
            next_control_attempt: 0,
            session: None,
            failed_generation: None,
        }
    }

    /// Format of the live producer session, if any.
    pub(crate) fn live_format(&mut self) -> Option<VideoFormat> {
        self.refresh(now_100ns());
        self.session.as_ref().filter(|s| s.alive && s.reader.state() != RING_STATE_STOPPED).map(|s| s.format)
    }

    /// The frame event and latest sequence of a live session that carries
    /// `want`'s pixel format and size, if it has a frame event.
    pub(crate) fn live_signal(&mut self, want: &VideoFormat) -> Option<LiveSignal> {
        self.refresh(now_100ns());
        let session = self.session.as_ref().filter(|s| s.serves(want) && s.reader.state() != RING_STATE_STOPPED)?;
        Some(LiveSignal {
            event: Arc::clone(session.frame_event.as_ref()?),
            frame: (session.reader.generation(), session.reader.latest_sequence()),
            producer: session.producer.clone(),
        })
    }

    pub(crate) fn producer_stopped(&mut self, generation: u64) {
        if let Some(session) = self.session.as_mut().filter(|session| session.generation == generation) {
            session.alive = false;
        }
    }

    /// Copies the newest producer frame into `dst` when a live session
    /// carries `want`'s pixel format and size, returning its sequence.
    /// `None` means the caller should deliver black instead.
    pub(crate) fn fill(&mut self, dst: &mut [u8], dst_layout: &FrameLayout, want: &VideoFormat, flip: bool) -> Option<FrameId> {
        self.refresh(now_100ns());
        let session = self.session.as_ref().filter(|s| s.serves(want))?;
        let mut copied = true;
        let outcome = session.reader.read_latest(|src| {
            // SAFETY: `src` spans the ring's validated tight frame layout.
            copied = unsafe { copy_frame(src, &session.source_layout, dst, dst_layout, flip) }.is_ok();
        });
        match outcome {
            ReadOutcome::Frame { sequence, .. } if copied => Some((session.reader.generation(), sequence)),
            _ => None,
        }
    }

    fn refresh(&mut self, now: u64) {
        if self.control.is_none() {
            if now < self.next_control_attempt {
                return;
            }
            self.next_control_attempt = now + CONTROL_RETRY_100NS;
            self.control = ReadView::open(&self.names.control).filter(|view| view.len() >= DshowControl::SIZE && view.as_ptr().align_offset(64) == 0);
        }
        let Some(control) = &self.control else { return };
        // SAFETY: The view is a live, aligned control mapping of sufficient size.
        let Some(snapshot) = (unsafe { DshowControl::snapshot(control.as_ptr().cast()) }) else {
            // A producer is rewriting the record; keep the current session.
            return;
        };
        if snapshot.state != CONTROL_STATE_ACTIVE {
            self.session = None;
            return;
        }
        if self.session.as_ref().is_none_or(|s| s.generation != snapshot.generation) {
            if self.failed_generation.is_some_and(|(generation, retry)| generation == snapshot.generation && now < retry) {
                self.session = None;
                return;
            }
            self.session = self.open_session(&snapshot, now);
            self.failed_generation = if self.session.is_none() { Some((snapshot.generation, now + SESSION_RETRY_100NS)) } else { None };
        }
        if let Some(session) = self.session.as_mut()
            && session.alive
            && now >= session.next_liveness_check
        {
            session.next_liveness_check = now + LIVENESS_PERIOD_100NS;
            if let Some(process) = &session.producer {
                // SAFETY: Live process handle with SYNCHRONIZE access.
                session.alive = unsafe { WaitForSingleObject(process.0, 0) } != WAIT_OBJECT_0;
            }
        }
    }

    fn open_session(&self, snapshot: &DshowControl, now: u64) -> Option<Session> {
        let format = snapshot.format.decode().ok()?;
        let validated = format.validate().ok()?;
        let view = ReadView::open(&self.names.ring(snapshot.generation))?;
        // SAFETY: The view stays mapped as long as the session (stored together).
        let reader = unsafe { RingReader::open(view.as_ptr(), view.len()) }.ok()?;
        if reader.generation() != snapshot.generation || reader.format() != format {
            return None;
        }
        let (producer, alive) = open_producer(snapshot.producer_pid, snapshot.producer_start_time);
        let event_name = self.names.frame_event(snapshot.generation);
        // SAFETY: NUL-terminated name; the handle is owned on success.
        let frame_event = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(event_name.as_ptr())) }.ok().map(|raw| Arc::new(Handle(raw)));
        Some(Session {
            generation: snapshot.generation,
            format,
            source_layout: *validated.layout(),
            reader,
            _view: view,
            producer: producer.map(Arc::new),
            frame_event,
            next_liveness_check: now + LIVENESS_PERIOD_100NS,
            alive,
        })
    }
}

/// Opens the producer process for liveness checks. A recycled process id
/// (different creation time) means the producer is gone. When access is
/// denied (e.g. different integrity level) liveness falls back to the
/// published control state.
fn open_producer(pid: u32, start_time: u64) -> (Option<Handle>, bool) {
    // SAFETY: Plain process open; the handle is owned on success.
    let Ok(raw) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return (None, true);
    };
    let process = Handle(raw);
    let mut times = [FILETIME::default(); 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: Live handle with query access and writable outputs.
    if unsafe { GetProcessTimes(process.0, creation, exit, kernel, user) }.is_ok() {
        let created = u64::from(times[0].dwHighDateTime) << 32 | u64::from(times[0].dwLowDateTime);
        if created != start_time {
            return (None, false);
        }
    }
    (Some(process), true)
}
