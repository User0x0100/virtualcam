//! The single video stream: request-driven, paced sample production.
//!
//! Frame Server (and every consumer behind it) asks for the next sample as
//! soon as the previous one arrives, so answering each `RequestSample` at
//! once would deliver as fast as the consumer can spin (thousands of frames
//! per second) and burn its CPU. A request is therefore answered only when it
//! is due:
//!
//! - a producer frame newer than the last delivered one is available in the
//!   selected mode, subject to the shared GCRA rate limit (see
//!   `virtualcam_protocol::delivery`);
//! - no usable producer frame exists (no producer, none written yet, or one
//!   written in a different mode than consumers selected): black at the
//!   selected mode's frame interval;
//! - a producer is attached but stalled: the newest frame again after
//!   one second, so consumers do not time out.
//!
//! A request that is not due is queued and answered by the stream's pacing
//! worker, which waits for the frame event, state changes or the delivery deadline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use virtualcam_protocol::delivery::Delivery;
use virtualcam_protocol::planes::{copy_frame, fill_black};
use virtualcam_protocol::ring::ReadOutcome;
use virtualcam_protocol::{FrameLayout, ValidatedFormat};
use windows::Win32::Foundation::{E_POINTER, E_UNEXPECTED, S_OK, WAIT_FAILED};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMF2DBuffer2, IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFMediaBuffer, IMFMediaEvent, IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaStream_Impl, IMFMediaStream2,
    IMFMediaStream2_Impl, IMFMediaType, IMFSample, IMFStreamDescriptor, IMFVideoSampleAllocator, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MEError, MEMediaSample, MEStreamStarted, MEStreamStopped, MF_E_INVALIDMEDIATYPE,
    MF_E_INVALIDREQUEST, MF_E_MEDIA_SOURCE_WRONGSTATE, MF_E_SHUTDOWN, MF_MT_SUBTYPE, MF_STREAM_STATE, MF_STREAM_STATE_PAUSED, MF_STREAM_STATE_RUNNING, MF_STREAM_STATE_STOPPED, MF2DBuffer_LockFlags_Write, MFGetSystemTime,
    MFSampleExtension_Token,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects};
use windows_core::{ComObject, GUID, IUnknown, IUnknownImpl, Interface, Ref, implement};

use crate::pool::SamplePool;
use crate::rendezvous::{Handle, TransportSlot};
use crate::{ModuleRef, guard};

/// Frames requested from a provided allocator.
const ALLOCATOR_FRAMES: u32 = 10;
fn now_100ns() -> u64 {
    // SAFETY: Plain monotonic MF clock read.
    unsafe { MFGetSystemTime() }.max(0) as u64
}

pub(crate) struct StreamInner {
    state: MF_STREAM_STATE,
    shutdown: bool,
    /// Strong back-reference for `GetMediaSource`; broken by `shutdown`.
    source: Option<IMFMediaSource>,
    allocator: Option<IMFVideoSampleAllocator>,
    allocator_ready: bool,
    /// Index into `MediaStream::offers` of the type selected at the last
    /// start.
    output: usize,
    /// Tokens of requests not answered yet, oldest first.
    pending: VecDeque<Option<IUnknown>>,
    delivery: Delivery,
    /// The pacing worker thread is running.
    worker: bool,
}

#[implement(IMFMediaStream2)]
pub(crate) struct MediaStream {
    pub(crate) queue: IMFMediaEventQueue,
    pub(crate) descriptor: IMFStreamDescriptor,
    pub(crate) attributes: IMFAttributes,
    /// Every mode the camera declared, in stream-descriptor order.
    offers: Vec<(ValidatedFormat, IMFMediaType)>,
    /// Frame interval shared by every offered mode.
    interval_100ns: u64,
    transport: Arc<TransportSlot>,
    /// Reused linear samples when no pipeline allocator is usable.
    samples: SamplePool,
    inner: Mutex<StreamInner>,
    _module: ModuleRef,
}

/// A `VT_I8` PROPVARIANT (event start times).
pub(crate) fn variant_i64(value: i64) -> PROPVARIANT {
    let mut variant = PROPVARIANT::default();
    // SAFETY: Writes the discriminant and the matching union member.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = windows::Win32::System::Variant::VT_I8;
        inner.Anonymous.hVal = value;
    }
    variant
}

impl MediaStream {
    /// `offers` must be non-empty; `default` indexes the mode consumers get
    /// when they select none.
    pub(crate) fn new(queue: IMFMediaEventQueue, descriptor: IMFStreamDescriptor, attributes: IMFAttributes, offers: Vec<(ValidatedFormat, IMFMediaType)>, default: usize, transport: Arc<TransportSlot>) -> Self {
        let interval_100ns = offers[default].0.interval_100ns();
        Self {
            queue,
            descriptor,
            attributes,
            offers,
            interval_100ns,
            transport,
            samples: SamplePool::new(),
            inner: Mutex::new(StreamInner {
                state: MF_STREAM_STATE_STOPPED,
                shutdown: false,
                source: None,
                allocator: None,
                allocator_ready: false,
                output: default,
                pending: VecDeque::new(),
                delivery: Delivery::default(),
                worker: false,
            }),
            _module: ModuleRef::new(),
        }
    }

    fn inner(&self) -> MutexGuard<'_, StreamInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn live(&self) -> windows_core::Result<MutexGuard<'_, StreamInner>> {
        let inner = self.inner();
        if inner.shutdown { Err(MF_E_SHUTDOWN.into()) } else { Ok(inner) }
    }

    pub(crate) fn set_source(&self, source: IMFMediaSource) {
        self.inner().source = Some(source);
    }

    pub(crate) fn is_running(&self) -> bool {
        self.inner().state == MF_STREAM_STATE_RUNNING
    }

    /// Stores a pipeline-provided allocator; refused while running.
    pub(crate) fn set_allocator(&self, allocator: Option<IMFVideoSampleAllocator>) -> windows_core::Result<()> {
        let mut inner = self.live()?;
        if inner.state == MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        if let Some(old) = inner.allocator.take()
            && inner.allocator_ready
        {
            // SAFETY: Valid allocator.
            let _ = unsafe { old.UninitializeSampleAllocator() };
        }
        inner.allocator = allocator;
        inner.allocator_ready = false;
        Ok(())
    }

    /// Index of the offered mode whose subtype `current` (the selected
    /// stream descriptor's current media type) carries.
    pub(crate) fn offer_of(&self, current: &IMFMediaType) -> windows_core::Result<usize> {
        // SAFETY: Valid media type.
        let subtype = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }?;
        self.offers
            .iter()
            .position(|(format, _)| format.format().pixel_format.mf_subtype().is_some_and(|offered| GUID::from_u128(offered) == subtype))
            .ok_or_else(|| MF_E_INVALIDMEDIATYPE.into())
    }

    /// Source -> stream: begin producing the offered mode `output` (from
    /// [`Self::offer_of`]). Queues `MEStreamStarted`.
    pub(crate) fn start(&self, time: i64, output: usize) -> windows_core::Result<()> {
        let mut inner = self.live()?;
        if output >= self.offers.len() {
            return Err(MF_E_INVALIDMEDIATYPE.into());
        }
        if inner.output != output {
            // The allocator was initialized for the previous type.
            if inner.allocator_ready
                && let Some(allocator) = &inner.allocator
            {
                // SAFETY: Valid allocator; reinitialized below.
                let _ = unsafe { allocator.UninitializeSampleAllocator() };
            }
            inner.allocator_ready = false;
            inner.output = output;
        }
        // Ask the producer for this mode so the source only copies.
        self.transport.select(self.offers[output].0.format());
        if let Some(allocator) = inner.allocator.clone()
            && !inner.allocator_ready
        {
            // SAFETY: Valid allocator and media type. A failing allocator is
            // dropped so the standard MF allocation path still works.
            if unsafe { allocator.InitializeSampleAllocator(ALLOCATOR_FRAMES, &self.offers[output].1) }.is_ok() {
                inner.allocator_ready = true;
            } else {
                inner.allocator = None;
            }
        }
        inner.state = MF_STREAM_STATE_RUNNING;
        self.transport.wake();
        // SAFETY: Valid queue; the variant outlives the call.
        unsafe { self.queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, &variant_i64(time)) }
    }

    /// Source -> stream: stop producing. Queues `MEStreamStopped`.
    pub(crate) fn stop(&self) -> windows_core::Result<()> {
        let mut inner = self.live()?;
        let was_running = inner.state != MF_STREAM_STATE_STOPPED;
        inner.state = MF_STREAM_STATE_STOPPED;
        // Requests of a stopped stream are never answered.
        inner.pending.clear();
        self.transport.wake();
        if inner.allocator_ready
            && let Some(allocator) = &inner.allocator
        {
            // SAFETY: Valid allocator; rebuilt on the next start.
            let _ = unsafe { allocator.UninitializeSampleAllocator() };
            inner.allocator_ready = false;
        }
        if was_running {
            // SAFETY: Valid queue.
            unsafe { self.queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, &PROPVARIANT::default()) }?;
        }
        Ok(())
    }

    /// Source -> stream: release everything and break the source cycle.
    pub(crate) fn shutdown(&self) {
        let mut inner = self.inner();
        if inner.shutdown {
            return;
        }
        inner.shutdown = true;
        inner.state = MF_STREAM_STATE_STOPPED;
        if let Some(allocator) = inner.allocator.take()
            && inner.allocator_ready
        {
            // SAFETY: Valid allocator.
            let _ = unsafe { allocator.UninitializeSampleAllocator() };
        }
        inner.allocator_ready = false;
        inner.source = None;
        inner.pending.clear();
        // The pacing worker exits and drops its reference to this stream.
        self.transport.wake();
        // SAFETY: Valid queue; later queue calls report MF_E_SHUTDOWN.
        let _ = unsafe { self.queue.Shutdown() };
    }

    /// Returns a sample whose buffer memory is laid out exactly as the tight
    /// `output` frame (`MF_MT_DEFAULT_STRIDE`). Frame Server's DirectShow bridge
    /// reads the raw memory with that stride and ignores a 2D buffer's own
    /// pitch, so rows padded for alignment (NV12 1080 wide -> 1088) arrive
    /// sheared. A pipeline allocator that hands out padded buffers is
    /// therefore dropped for the rest of this run in favor of linear buffers.
    fn allocate(inner: &mut StreamInner, samples: &SamplePool, output: &ValidatedFormat) -> windows_core::Result<IMFSample> {
        if inner.allocator_ready
            && let Some(allocator) = inner.allocator.clone()
        {
            // SAFETY: Initialized allocator.
            let sample = unsafe { allocator.AllocateSample() }?;
            if Self::is_tight(&sample, output)? {
                return Ok(sample);
            }
            // Return the sample before tearing the allocator down.
            drop(sample);
            // SAFETY: Valid, initialized allocator with no sample outstanding from this call.
            let _ = unsafe { allocator.UninitializeSampleAllocator() };
            inner.allocator = None;
            inner.allocator_ready = false;
        }
        // A linear buffer: its memory is the tight frame by construction.
        samples.take(output.frame_size())
    }

    /// Whether the sample's first buffer is linear or a 2D buffer whose pitch
    /// equals the tight top-down first-plane pitch.
    fn is_tight(sample: &IMFSample, output: &ValidatedFormat) -> windows_core::Result<bool> {
        // SAFETY: Valid sample.
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() else {
            return Ok(true);
        };
        let (mut scanline0, mut pitch) = (core::ptr::null_mut(), 0_i32);
        // SAFETY: Valid buffer; outputs writable.
        unsafe { buffer2d.Lock2D(&mut scanline0, &mut pitch) }?;
        // SAFETY: Balances the successful lock.
        let _ = unsafe { buffer2d.Unlock2D() };
        Ok(usize::try_from(pitch).is_ok_and(|pitch| pitch == output.layout().planes()[0].pitch))
    }

    /// Copies the newest producer frame when it is in exactly the selected
    /// mode, or writes black, into `buffer` as `output`, honoring its pitch,
    /// which may differ from the tight layout and may be negative for
    /// bottom-up RGB.
    /// Returns the `(ring generation, sequence)` of the processed producer
    /// frame, even when a frame in another mode is delivered as black.
    fn fill(&self, buffer: &IMFMediaBuffer, output: &ValidatedFormat) -> windows_core::Result<Option<(u64, u64)>> {
        if let Ok(buffer2) = buffer.cast::<IMF2DBuffer2>() {
            let (mut scanline0, mut pitch, mut start, mut length) = (core::ptr::null_mut(), 0_i32, core::ptr::null_mut(), 0_u32);
            // SAFETY: Valid buffer; all outputs writable.
            unsafe { buffer2.Lock2DSize(MF2DBuffer_LockFlags_Write, &mut scanline0, &mut pitch, &mut start, &mut length) }?;
            let result = self.write_region(output, start, length as usize, scanline0, pitch);
            // SAFETY: Balances the successful lock on every path.
            let _ = unsafe { buffer2.Unlock2D() };
            return result;
        }
        if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
            let (mut scanline0, mut pitch) = (core::ptr::null_mut(), 0_i32);
            // SAFETY: Valid buffer; outputs writable.
            unsafe { buffer2d.Lock2D(&mut scanline0, &mut pitch) }?;
            // SAFETY: Valid 2D buffer.
            let length = unsafe { buffer2d.GetContiguousLength() }.unwrap_or(0) as usize;
            let region = if pitch < 0 {
                scanline0.wrapping_offset((output.format().height as isize - 1) * pitch as isize)
            } else {
                scanline0
            };
            let result = self.write_region(output, region, length, scanline0, pitch);
            // SAFETY: Balances the successful lock on every path.
            let _ = unsafe { buffer2d.Unlock2D() };
            return result;
        }
        let (mut data, mut max_length) = (core::ptr::null_mut(), 0_u32);
        // SAFETY: Valid buffer; outputs writable.
        unsafe { buffer.Lock(&mut data, Some(&mut max_length), None) }?;
        let pitch = output.layout().planes()[0].pitch as i32;
        let result = self.write_region(output, data, max_length as usize, data, pitch);
        // SAFETY: Balances the successful lock on every path.
        let _ = unsafe { buffer.Unlock() };
        if result.is_ok() {
            // SAFETY: Valid buffer.
            unsafe { buffer.SetCurrentLength(output.frame_size() as u32) }?;
        }
        result
    }

    fn write_region(&self, output: &ValidatedFormat, start: *mut u8, length: usize, scanline0: *mut u8, pitch: i32) -> windows_core::Result<Option<(u64, u64)>> {
        let format = output.format();
        let invalid = || windows_core::Error::from(E_UNEXPECTED);
        if start.is_null() || scanline0.is_null() || pitch == 0 {
            return Err(E_POINTER.into());
        }
        let flip = pitch < 0;
        let magnitude = pitch.unsigned_abs() as usize;
        // The lowest-addressed row: the top row for top-down buffers, the
        // bottom row for negative pitch.
        let lowest = if flip { scanline0.wrapping_offset(-((format.height as isize - 1) * magnitude as isize)) } else { scanline0 };
        let offset = (lowest as usize).checked_sub(start as usize).ok_or_else(invalid)?;
        let layout: FrameLayout = format.pixel_format.layout_with_pitch(format.width, format.height, magnitude).map_err(|_| invalid())?;
        if flip && layout.planes().len() != 1 {
            return Err(invalid());
        }
        let available = length.checked_sub(offset).ok_or_else(invalid)?;
        if layout.size() > available {
            return Err(invalid());
        }
        // SAFETY: `lowest..lowest + layout.size()` lies inside the locked
        // buffer region `start..start + length`, validated above.
        let destination = unsafe { core::slice::from_raw_parts_mut(lowest, layout.size()) };
        let mut copied = true;
        let outcome = self.transport.read_latest(|src, producing| {
            // Only a frame in the selected mode is delivered; one in another
            // declared mode is never reinterpreted, resampled or converted.
            copied = producing.format() == format
                // SAFETY: `src` spans the ring's validated tight frame.
                && unsafe { copy_frame(src, producing.layout(), destination, &layout, flip) }.is_ok();
        });
        let frame = match outcome {
            Some((generation, ReadOutcome::Frame { sequence, .. })) => Some((generation, sequence)),
            _ => None,
        };
        if !copied || frame.is_none() {
            fill_black(destination, &layout, format.pixel_format, format.color.is_full_range(format.pixel_format)).map_err(|_| invalid())?;
        }
        Ok(frame)
    }

    /// When the oldest pending request may be answered (see the module docs).
    fn due(&self, inner: &mut StreamInner, now: u64) -> u64 {
        inner.delivery.due(now, self.transport.latest_frame(), self.interval_100ns)
    }

    /// Answers the oldest pending request with a filled sample.
    fn deliver(&self, inner: &mut StreamInner) -> windows_core::Result<()> {
        let Some(token) = inner.pending.pop_front() else { return Ok(()) };
        let output = self.offers[inner.output].0;
        let sample = Self::allocate(inner, &self.samples, &output)?;
        // SAFETY: Valid sample with at least one buffer.
        let buffer = unsafe { sample.GetBufferByIndex(0) }?;
        let frame = self.fill(&buffer, &output)?;
        inner.delivery.delivered(now_100ns(), frame, self.interval_100ns);
        // SAFETY: Valid sample.
        unsafe {
            sample.SetSampleTime(MFGetSystemTime())?;
            sample.SetSampleDuration(self.interval_100ns as i64)?;
            if let Some(token) = &token {
                sample.SetUnknown(&MFSampleExtension_Token, token)?;
            }
            self.queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)
        }
    }

    /// Body of the pacing worker: answers pending requests as they fall due
    /// until the stream shuts down.
    fn pace(&self) {
        let mut inner = self.inner();
        let fail = |inner: &mut StreamInner, error: windows_core::Error| {
            inner.worker = false;
            // SAFETY: Valid queue, or shutdown reports its own error.
            let _ = unsafe { self.queue.QueueEventParamVar(MEError.0 as u32, &GUID::zeroed(), error.code(), &PROPVARIANT::default()) };
        };
        // Millisecond wait timeouts round to the service's coarse clock tick.
        // SAFETY: Anonymous high-resolution timer, owned by this worker.
        let timer = match unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) } {
            Ok(timer) => Handle(timer),
            Err(error) => {
                fail(&mut inner, error);
                return;
            }
        };
        loop {
            if inner.shutdown {
                inner.worker = false;
                return;
            }
            let (frame, timed) = if inner.state == MF_STREAM_STATE_RUNNING && !inner.pending.is_empty() {
                // Keep this session's event alive across the wait, even on detach.
                let frame = self.transport.frame_event();
                let now = now_100ns();
                let due = self.due(&mut inner, now);
                if due <= now {
                    if let Err(error) = self.deliver(&mut inner) {
                        // Asynchronous failure: reported as a stream error.
                        // SAFETY: Valid queue.
                        let _ = unsafe { self.queue.QueueEventParamVar(MEError.0 as u32, &GUID::zeroed(), error.code(), &PROPVARIANT::default()) };
                    }
                    continue;
                }
                let relative = -((due - now).min(i64::MAX as u64) as i64);
                // SAFETY: Live timer and a relative deadline in 100 ns units.
                if let Err(error) = unsafe { SetWaitableTimer(timer.0, &relative, 0, None, None, false) } {
                    fail(&mut inner, error);
                    return;
                }
                (frame, true)
            } else {
                (None, false)
            };
            let wake = self.transport.wake.0;
            let handles = [wake, timer.0, frame.as_ref().map_or(timer.0, |event| event.0)];
            let count = 1 + usize::from(timed) + usize::from(frame.is_some());
            drop(inner);
            // SAFETY: Distinct live handles; `frame` retains a detached session's event.
            let waited = unsafe { WaitForMultipleObjects(&handles[..count], false, INFINITE) };
            let error = (waited == WAIT_FAILED).then(windows_core::Error::from_thread);
            inner = self.inner();
            if let Some(error) = error {
                fail(&mut inner, error);
                return;
            }
        }
    }
}

/// The stream object moved to its pacing worker.
struct Worker(ComObject<MediaStream>);

// SAFETY: Every field of `MediaStream` is a free-threaded Media Foundation
// object or lock-protected state; Frame Server already calls the stream from
// arbitrary MTA threads.
unsafe impl Send for Worker {}

impl Worker {
    fn run(self) {
        // SAFETY: Joins the process MTA for the MF calls of this thread.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let _ = guard(|| {
            self.0.pace();
            Ok(())
        });
        drop(self);
        if initialized {
            // SAFETY: Balances the successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

impl MediaStream_Impl {
    /// Starts the pacing worker once; it lives until the stream shuts down.
    fn ensure_worker(&self, inner: &mut StreamInner) {
        if inner.worker {
            return;
        }
        let worker = Worker(self.to_object());
        let started = std::thread::Builder::new().name("virtualcam-mf-pacing".into()).spawn(move || worker.run());
        inner.worker = started.is_ok();
    }
}

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows_core::Result<IMFMediaEvent> {
        // Check shutdown under the lock, then block with no lock held.
        drop(self.live()?);
        // SAFETY: Valid queue (it reports MF_E_SHUTDOWN itself if shut down meanwhile).
        unsafe { self.queue.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows_core::Result<()> {
        drop(self.live()?);
        // SAFETY: Valid queue and forwarded references.
        unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows_core::Result<IMFMediaEvent> {
        drop(self.live()?);
        // SAFETY: Valid queue and forwarded reference.
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(&self, met: u32, extended: *const GUID, status: windows_core::HRESULT, value: *const PROPVARIANT) -> windows_core::Result<()> {
        drop(self.live()?);
        // SAFETY: Forwarded arguments.
        unsafe { self.queue.QueueEventParamVar(met, extended, status, value) }
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> windows_core::Result<IMFMediaSource> {
        // Copy the parent pointer only; never call into the source here.
        self.live()?.source.clone().ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> windows_core::Result<IMFStreamDescriptor> {
        drop(self.live()?);
        Ok(self.descriptor.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> windows_core::Result<()> {
        guard(|| {
            let mut inner = self.live()?;
            if inner.state != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
            }
            inner.pending.push_back(token.as_ref().cloned());
            // Answer at once when due and nothing older waits; failures then
            // return their HRESULT here and are never also queued as MEError.
            let now = now_100ns();
            if inner.pending.len() == 1 && self.due(&mut inner, now) <= now {
                return self.deliver(&mut inner);
            }
            self.ensure_worker(&mut inner);
            self.transport.wake();
            Ok(())
        })
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> windows_core::Result<()> {
        let mut inner = self.live()?;
        match value {
            MF_STREAM_STATE_STOPPED | MF_STREAM_STATE_PAUSED => {
                inner.state = value;
                if value == MF_STREAM_STATE_STOPPED {
                    inner.pending.clear();
                }
                self.transport.wake();
                Ok(())
            }
            // Running is entered only through the source's Start, which
            // also initializes the allocator and queues the events.
            MF_STREAM_STATE_RUNNING if inner.state == MF_STREAM_STATE_RUNNING => Ok(()),
            _ => Err(MF_E_INVALIDREQUEST.into()),
        }
    }

    fn GetStreamState(&self) -> windows_core::Result<MF_STREAM_STATE> {
        Ok(self.live()?.state)
    }
}
