//! The push thread: delivers producer frames as they are published.
//!
//! While a live producer serves the connected format, the thread waits on
//! the session's frame event and delivers each new frame immediately, so
//! the source adds no pacing latency and never beats against the
//! producer's cadence (duplicated / skipped frames). Delivery is capped
//! at the negotiated rate (shared GCRA schedule), and a stalled producer's last frame is
//! repeated every second. Without a live producer (black) or with a
//! producer lacking a frame event, a timer paces at the negotiated rate.
//!
//! Each delivery takes one allocator buffer, copies the newest producer
//! frame straight into it (or writes black), stamps stream times, and calls
//! `IMemInputPin::Receive`. Missed deadlines are skipped instead of being
//! caught up in a burst. Paused graphs receive nothing (live source).

use std::sync::Arc;
use std::thread::JoinHandle;

use virtualcam_protocol::VideoFormat;
use virtualcam_protocol::delivery::Delivery;
use virtualcam_protocol::planes::fill_black;
use windows::Win32::Foundation::{HANDLE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::DirectShow::{IMediaSample, State_Running, State_Stopped};
use windows::Win32::System::Threading::{CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE, SetEvent, SetWaitableTimer, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject};
use windows_core::Interface;

use crate::filter::Shared;
use crate::win::{Handle, now_100ns};
use crate::{ModuleRef, guard};

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

/// Waits for the shared delivery deadline, a new frame, or a state change.
/// `None` stops the worker; `Some(false)` rechecks the filter state.
fn wait_for_delivery(shared: &Shared, format: &VideoFormat, stop: &Handle, wake: &Handle, timer: &Handle, delivery: &Delivery, interval: u64) -> Option<bool> {
    loop {
        let live = shared.transport().live_signal(format);
        let now = now_100ns();
        let due = delivery.due(now, live.as_ref().map(|signal| signal.frame), interval);
        if due <= now {
            return Some(true);
        }
        let relative = -((due - now).min(i64::MAX as u64) as i64);
        // SAFETY: Live high-resolution timer, relative deadline in 100 ns units.
        if unsafe { SetWaitableTimer(timer.0, &relative, 0, None, None, false) }.is_err() {
            return None;
        }
        let process = live.as_ref().and_then(|signal| signal.producer.as_ref());
        let handles = [stop.0, wake.0, timer.0, live.as_ref().map_or(timer.0, |signal| signal.event.0), process.map_or(timer.0, |process| process.0)];
        let count = 3 + usize::from(live.is_some()) + usize::from(process.is_some());
        // SAFETY: Distinct live handles; the session event is retained by `live`.
        match unsafe { WaitForMultipleObjects(&handles[..count], false, INFINITE) } {
            WAIT_OBJECT_0 => return None,
            result if result.0 == WAIT_OBJECT_0.0 + 1 => return Some(false),
            result if result.0 == WAIT_OBJECT_0.0 + 2 || result.0 == WAIT_OBJECT_0.0 + 3 => {}
            result if result.0 == WAIT_OBJECT_0.0 + 4 => {
                if let Some(signal) = &live {
                    shared.transport().producer_stopped(signal.frame.0);
                }
            }
            _ => return None,
        }
    }
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
    let mut delivery = Delivery::default();
    let mut discontinuity = true;
    let mut downstream_refused = false;

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
            unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            delivery = Delivery::default();
            discontinuity = true;
            continue;
        }

        match wait_for_delivery(shared, &format, stop, wake, timer, &delivery, interval) {
            None => return,
            Some(true) => {}
            Some(false) => continue,
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
        let frame = shared.transport().fill(buffer, &layout, &format, flip);
        if frame.is_none() {
            let _ = fill_black(buffer, &layout, format.pixel_format, full_range);
        }
        delivery.delivered(now_100ns(), frame, interval);

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
