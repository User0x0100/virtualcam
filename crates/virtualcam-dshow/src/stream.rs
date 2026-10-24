//! The push thread: delivers producer frames as they are published.
//!
//! While a live producer serves the connected format, the thread waits on
//! the session's frame event and delivers each new frame immediately, so
//! the source adds no pacing latency and never beats against the
//! producer's cadence (duplicated / skipped frames). Delivery is capped
//! near the negotiated rate, and a stalled producer's last frame is
//! repeated every two intervals. Without a live producer (black) or with a
//! producer lacking a frame event, a timer paces at the negotiated rate.
//!
//! Each delivery takes one allocator buffer, copies the newest producer
//! frame straight into it (or writes black), stamps stream times, and calls
//! `IMemInputPin::Receive`. Missed deadlines are skipped instead of being
//! caught up in a burst. Paused graphs receive nothing (live source).

use std::sync::Arc;
use std::thread::JoinHandle;

use virtualcam_protocol::VideoFormat;
use virtualcam_protocol::planes::fill_black;
use windows::Win32::Foundation::{HANDLE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::DirectShow::{IMediaSample, State_Running, State_Stopped};
use windows::Win32::System::Threading::{CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject};
use windows_core::Interface;

use crate::filter::Shared;
use crate::win::{Handle, now_100ns};
use crate::{ModuleRef, guard};

const PAUSED_POLL_MS: u32 = 50;

pub(crate) struct Worker {
    thread: JoinHandle<()>,
    stop: Arc<Handle>,
    wake: Arc<Handle>,
}

impl Worker {
    pub(crate) fn spawn(shared: Arc<Shared>) -> windows_core::Result<Self> {
        // SAFETY: Anonymous manual-reset (stop) and auto-reset (wake) events.
        let stop = Arc::new(Handle(unsafe { CreateEventW(None, true, false, None) }?));
        // SAFETY: As above.
        let wake = Arc::new(Handle(unsafe { CreateEventW(None, false, false, None) }?));
        // SAFETY: Anonymous high-resolution waitable timer.
        let timer = Handle(unsafe { CreateWaitableTimerExW(None, None, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0) }?);
        let (thread_stop, thread_wake) = (Arc::clone(&stop), Arc::clone(&wake));
        // The module reference moves into the thread so the DLL stays loaded
        // until the thread has finished running our code.
        let module = ModuleRef::new();
        let thread = std::thread::Builder::new()
            .name("virtualcam-dshow-push".into())
            .spawn(move || {
                let _module = module;
                let _ = guard(|| {
                    run(&shared, &thread_stop, &thread_wake, &timer);
                    Ok(())
                });
            })
            .map_err(|_| windows_core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?;
        Ok(Self { thread, stop, wake })
    }

    /// Wakes a paused worker after `Run`.
    pub(crate) fn wake(&self) {
        // SAFETY: Live event handle.
        let _ = unsafe { SetEvent(self.wake.0) };
    }

    /// Signals stop and waits for the thread. Must not be called with the
    /// filter state lock held.
    pub(crate) fn stop_and_join(self) {
        // SAFETY: Live event handle.
        let _ = unsafe { SetEvent(self.stop.0) };
        let _ = self.thread.join();
    }
}

fn stopped(stop: &Handle) -> bool {
    // SAFETY: Live event handle.
    unsafe { WaitForSingleObject(stop.0, 0) == WAIT_OBJECT_0 }
}

/// What woke a frame-event wait.
enum Wake {
    Stop,
    Frame,
    Timeout,
}

/// Waits for the frame event, stop, or `due` (QPC 100 ns).
fn wait_frame(stop: &Handle, frame: &Handle, due: u64) -> Wake {
    let timeout_ms = due.saturating_sub(now_100ns()).div_ceil(10_000).min(u64::from(u32::MAX - 1)) as u32;
    let handles: [HANDLE; 2] = [stop.0, frame.0];
    // SAFETY: Two live handles.
    match unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) } {
        WAIT_OBJECT_0 => Wake::Stop,
        result if result.0 == WAIT_OBJECT_0.0 + 1 => Wake::Frame,
        _ => Wake::Timeout,
    }
}

/// Waits for the next delivery while a frame-event producer serves the
/// connection: a new frame (not before `earliest`), or `repeat_at` for a
/// stalled one. Returns `None` on stop, `Some(false)` when the producer
/// went away (fall back to the timer), `Some(true)` to deliver.
fn wait_for_frame(shared: &Shared, format: &VideoFormat, stop: &Handle, timer: &Handle, delivered: u64, earliest: u64, repeat_at: u64) -> Option<bool> {
    loop {
        let Some((event, latest)) = shared.transport().live_signal(format) else { return Some(false) };
        if latest != delivered {
            // New frame: respect the rate cap, then deliver the newest.
            return wait_until(stop, timer, earliest).then_some(true);
        }
        match wait_frame(stop, &event, repeat_at) {
            Wake::Stop => return None,
            // The event may still be set for the frame already delivered;
            // the sequence check above filters that out.
            Wake::Frame => {}
            Wake::Timeout => return Some(true),
        }
        if now_100ns() >= repeat_at {
            return Some(true);
        }
    }
}

/// Waits until `due` (QPC 100 ns) or stop; returns `false` on stop.
fn wait_until(stop: &Handle, timer: &Handle, due: u64) -> bool {
    let now = now_100ns();
    if due > now {
        let relative = -((due - now) as i64);
        // SAFETY: Live timer; relative due time in 100 ns units.
        if unsafe { SetWaitableTimer(timer.0, &relative, 0, None, None, false) }.is_ok() {
            let handles: [HANDLE; 2] = [stop.0, timer.0];
            // SAFETY: Two live handles.
            return unsafe { WaitForMultipleObjects(&handles, false, INFINITE) } != WAIT_OBJECT_0;
        }
    }
    !stopped(stop)
}

fn run(shared: &Shared, stop: &Handle, wake: &Handle, timer: &Handle) {
    let Some((input, allocator, format, layout, flip, interval)) = ({
        let state = shared.state();
        state
            .connection
            .as_ref()
            .map(|c| (c.input.0.clone(), c.allocator.0.clone(), c.format, c.layout, c.flip, c.format.frame_rate.interval_100ns().unwrap_or(333_333)))
    }) else {
        return;
    };
    let full_range = format.color.is_full_range(format.pixel_format);
    let mut next_due: Option<u64> = None;
    let mut discontinuity = true;
    let mut downstream_refused = false;
    // Sequence and QPC time of the last delivered producer frame.
    let mut delivered_sequence = 0_u64;
    let mut delivered_at = 0_u64;

    while !stopped(stop) {
        let (filter_state, start_time, run_qpc, clock) = {
            let state = shared.state();
            (state.filter_state, state.start_time, state.run_qpc, state.clock.as_ref().map(|c| c.0.clone()))
        };
        if filter_state == State_Stopped {
            return;
        }
        if filter_state != State_Running || downstream_refused {
            // Paused (or downstream stopped accepting): deliver nothing.
            let handles: [HANDLE; 2] = [stop.0, wake.0];
            // SAFETY: Two live event handles.
            unsafe { WaitForMultipleObjects(&handles, false, PAUSED_POLL_MS) };
            next_due = None;
            discontinuity = true;
            continue;
        }

        // Frame-driven while a live producer serves this connection; the
        // cap tolerates a quarter interval of producer jitter.
        let earliest = delivered_at + interval - interval / 4;
        let repeat_at = delivered_at + 2 * interval;
        match wait_for_frame(shared, &format, stop, timer, delivered_sequence, earliest, repeat_at) {
            None => return,
            Some(true) => next_due = None,
            Some(false) => {
                let due = next_due.unwrap_or_else(now_100ns);
                if !wait_until(stop, timer, due) {
                    return;
                }
                let now = now_100ns();
                // Skip missed slots rather than bursting to catch up.
                let next = due + interval;
                next_due = Some(if next <= now { now + interval } else { next });
            }
        }

        let mut sample: Option<IMediaSample> = None;
        // SAFETY: Valid allocator; blocks until a buffer is free or decommit.
        if unsafe { allocator.GetBuffer(&mut sample, None, None, 0) }.is_err() {
            if stopped(stop) {
                return;
            }
            continue;
        }
        let Some(sample) = sample else { continue };
        // SAFETY: Valid sample from our committed allocator.
        let (Ok(pointer), size) = (unsafe { sample.GetPointer() }, unsafe { sample.GetSize() }) else { continue };
        if pointer.is_null() || size < 0 || (size as usize) < layout.size() {
            continue;
        }
        // SAFETY: The allocator buffer spans `size` writable bytes while we hold the sample.
        let buffer = unsafe { core::slice::from_raw_parts_mut(pointer, size as usize) };
        match shared.transport().fill(buffer, &layout, &format, flip) {
            Some(sequence) => delivered_sequence = sequence,
            None => {
                let _ = fill_black(buffer, &layout, format.pixel_format, full_range);
            }
        }
        delivered_at = now_100ns();

        // Stream time: reference clock relative to tStart, else QPC since Run.
        // SAFETY: Valid clock interface.
        let stream_time = clock
            .and_then(|clock| unsafe { clock.GetTime() }.ok())
            .map_or_else(|| now_100ns().saturating_sub(run_qpc) as i64, |time| time - start_time)
            .max(0);
        let end_time = stream_time + interval as i64;
        // SAFETY: Valid sample; time pointers outlive the calls.
        unsafe {
            let _ = sample.SetTime(Some(&stream_time), Some(&end_time));
            let _ = sample.SetSyncPoint(true);
            let _ = sample.SetDiscontinuity(discontinuity);
            let _ = sample.SetActualDataLength(layout.size() as i32);
        }
        discontinuity = false;
        // Call through the vtable to see S_FALSE ("stop sending"), which the
        // `Result` wrapper would fold into success.
        // SAFETY: Valid interfaces for the duration of the call.
        let hr = unsafe { (Interface::vtable(&input).Receive)(input.as_raw(), sample.as_raw()) };
        if hr != S_OK {
            downstream_refused = true;
        }
    }
}
