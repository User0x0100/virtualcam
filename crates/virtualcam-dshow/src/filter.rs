//! The source filter object and the state it shares with its pin and worker.
//!
//! `IPersist -> IMediaFilter -> IBaseFilter` are declared here with raw
//! `HRESULT` signatures (instead of the `windows` crate's `Result` shims) so
//! `GetState` can return the success code `VFW_S_CANT_CUE` together with the
//! paused state, which live sources must do so the graph does not wait for
//! a preroll sample that never comes.

use core::ffi::c_void;
use std::sync::{Arc, Mutex, MutexGuard};

use virtualcam_protocol::dshow::PIN_NAME;
use virtualcam_protocol::{FrameLayout, VideoFormat};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, S_OK};
use windows::Win32::Media::DirectShow::{
    AM_FILTER_MISC_FLAGS_IS_SOURCE, FILTER_INFO, FILTER_STATE, IAMFilterMiscFlags, IAMFilterMiscFlags_Impl, IEnumPins, IFilterGraph, IMemAllocator, IMemInputPin, IPin, State_Paused, State_Running, State_Stopped, VFW_E_NOT_FOUND,
    VFW_S_CANT_CUE,
};
use windows::Win32::Media::IReferenceClock;
use windows_core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, implement};

use crate::config::CameraConfig;
use crate::media_type::MediaType;
use crate::pin::{OutputPin, PinEnumerator};
use crate::stream::Worker;
use crate::transport::Transport;
use crate::{ModuleRef, guard_hr};

pub(crate) use raw::{IBaseFilterRaw, IBaseFilterRaw_Impl, IMediaFilterRaw_Impl, IPersistRaw_Impl};

#[allow(non_snake_case)]
mod raw {
    use core::ffi::c_void;

    use windows::Win32::Media::DirectShow::{FILTER_INFO, FILTER_STATE};
    use windows_core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, PWSTR, interface};

    #[interface("0000010C-0000-0000-C000-000000000046")]
    pub(crate) unsafe trait IPersistRaw: IUnknown {
        fn GetClassID(&self, clsid: *mut GUID) -> HRESULT;
    }

    #[interface("56A86899-0AD4-11CE-B03A-0020AF0BA770")]
    pub(crate) unsafe trait IMediaFilterRaw: IPersistRaw {
        fn Stop(&self) -> HRESULT;
        fn Pause(&self) -> HRESULT;
        fn Run(&self, start: i64) -> HRESULT;
        fn GetState(&self, timeout_ms: u32, state: *mut FILTER_STATE) -> HRESULT;
        fn SetSyncSource(&self, clock: *mut c_void) -> HRESULT;
        fn GetSyncSource(&self, clock: *mut *mut c_void) -> HRESULT;
    }

    #[interface("56A86895-0AD4-11CE-B03A-0020AF0BA770")]
    pub(crate) unsafe trait IBaseFilterRaw: IMediaFilterRaw {
        fn EnumPins(&self, pins: *mut *mut c_void) -> HRESULT;
        fn FindPin(&self, id: PCWSTR, pin: *mut *mut c_void) -> HRESULT;
        fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> HRESULT;
        fn JoinFilterGraph(&self, graph: *mut c_void, name: PCWSTR) -> HRESULT;
        fn QueryVendorInfo(&self, info: *mut PWSTR) -> HRESULT;
    }
}

/// A COM pointer moved across threads. DirectShow graph objects (pins,
/// allocators, clocks) are free-threaded.
pub(crate) struct Agile<T>(pub(crate) T);

// SAFETY: See the type documentation; only free-threaded DirectShow objects
// are wrapped.
unsafe impl<T> Send for Agile<T> {}

/// Everything fixed by a successful pin connection.
pub(crate) struct Connection {
    pub(crate) peer: Agile<IPin>,
    pub(crate) input: Agile<IMemInputPin>,
    pub(crate) allocator: Agile<IMemAllocator>,
    pub(crate) media_type: MediaType,
    pub(crate) format: VideoFormat,
    pub(crate) layout: FrameLayout,
    /// Bottom-up RGB DIB: rows are written in reverse order.
    pub(crate) flip: bool,
}

pub(crate) struct State {
    pub(crate) filter_state: FILTER_STATE,
    /// `tStart` passed to `Run`, in reference-clock time.
    pub(crate) start_time: i64,
    /// QPC (100 ns) at `Run`, for graphs without a reference clock.
    pub(crate) run_qpc: u64,
    pub(crate) clock: Option<Agile<IReferenceClock>>,
    /// Non-owning graph pointer, per the DirectShow rule that filters do not
    /// hold a reference on their graph.
    pub(crate) graph: usize,
    pub(crate) graph_name: [u16; 128],
    pub(crate) connection: Option<Connection>,
    /// Format selected through `IAMStreamConfig::SetFormat`.
    pub(crate) preferred: Option<VideoFormat>,
    pub(crate) worker: Option<Worker>,
}

pub(crate) struct Shared {
    pub(crate) camera: CameraConfig,
    pub(crate) state: Mutex<State>,
    pub(crate) transport: Mutex<Transport>,
}

impl Shared {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn transport(&self) -> MutexGuard<'_, Transport> {
        self.transport.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The single format offered while unconnected: an explicit
    /// `SetFormat`, else the live producer's, else the idle format.
    pub(crate) fn advertised(&self) -> VideoFormat {
        if let Some(preferred) = self.state().preferred {
            return preferred;
        }
        self.transport().live_format().unwrap_or_else(|| self.camera.idle_format())
    }
}

#[implement(IBaseFilterRaw, IAMFilterMiscFlags)]
pub(crate) struct SourceFilter {
    shared: Arc<Shared>,
    pin: Mutex<Option<IPin>>,
    _module: ModuleRef,
}

impl SourceFilter {
    /// Creates the filter and its single output pin.
    pub(crate) fn create(camera: CameraConfig) -> IUnknown {
        let shared = Arc::new(Shared {
            transport: Mutex::new(Transport::new(camera.id)),
            camera,
            state: Mutex::new(State {
                filter_state: State_Stopped,
                start_time: 0,
                run_qpc: 0,
                clock: None,
                graph: 0,
                graph_name: [0; 128],
                connection: None,
                preferred: None,
                worker: None,
            }),
        });
        let filter = windows_core::ComObject::new(Self {
            shared: Arc::clone(&shared),
            pin: Mutex::new(None),
            _module: ModuleRef::new(),
        });
        let unknown: IUnknown = filter.to_interface();
        // The filter owns its pin; the pin refers back weakly, so a graph that
        // releases the filter while holding the pin never leaks or dangles.
        let pin = OutputPin::create(shared, unknown.downgrade().ok());
        *filter.pin.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pin);
        unknown
    }

    fn pin(&self) -> Option<IPin> {
        self.pin.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    fn stop(&self) {
        let (worker, allocator) = {
            let mut state = self.shared.state();
            state.filter_state = State_Stopped;
            (state.worker.take(), state.connection.as_ref().map(|c| Agile(c.allocator.0.clone())))
        };
        // Outside the lock: decommit unblocks a worker waiting in GetBuffer,
        // then the join completes.
        if let Some(allocator) = allocator {
            // SAFETY: Valid allocator interface.
            let _ = unsafe { allocator.0.Decommit() };
        }
        if let Some(worker) = worker {
            worker.stop_and_join();
        }
    }

    fn pause(&self) -> HRESULT {
        let mut state = self.shared.state();
        if state.filter_state == State_Stopped
            && let Some(connection) = &state.connection
        {
            // SAFETY: Valid allocator interface.
            if let Err(error) = unsafe { connection.allocator.0.Commit() } {
                return error.code();
            }
            match Worker::spawn(Arc::clone(&self.shared)) {
                Ok(worker) => state.worker = Some(worker),
                Err(error) => return error.code(),
            }
        }
        state.filter_state = State_Paused;
        S_OK
    }
}

impl Drop for SourceFilter {
    fn drop(&mut self) {
        self.stop();
        // Break the connection so the peer pin is released with us.
        self.shared.state().connection = None;
    }
}

impl IPersistRaw_Impl for SourceFilter_Impl {
    unsafe fn GetClassID(&self, clsid: *mut GUID) -> HRESULT {
        guard_hr(|| {
            if clsid.is_null() {
                return E_POINTER;
            }
            // SAFETY: Non-null caller out pointer.
            unsafe { *clsid = GUID::from_u128(self.shared.camera.id.to_u128()) };
            S_OK
        })
    }
}

impl IMediaFilterRaw_Impl for SourceFilter_Impl {
    unsafe fn Stop(&self) -> HRESULT {
        guard_hr(|| {
            self.stop();
            S_OK
        })
    }

    unsafe fn Pause(&self) -> HRESULT {
        guard_hr(|| self.pause())
    }

    unsafe fn Run(&self, start: i64) -> HRESULT {
        guard_hr(|| {
            if self.shared.state().filter_state == State_Stopped {
                let hr = self.pause();
                if hr.is_err() {
                    return hr;
                }
            }
            let mut state = self.shared.state();
            state.start_time = start;
            state.run_qpc = crate::win::now_100ns();
            state.filter_state = State_Running;
            if let Some(worker) = &state.worker {
                worker.wake();
            }
            S_OK
        })
    }

    unsafe fn GetState(&self, _timeout_ms: u32, out: *mut FILTER_STATE) -> HRESULT {
        guard_hr(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let state = self.shared.state().filter_state;
            // SAFETY: Non-null caller out pointer.
            unsafe { *out = state };
            if state == State_Paused { VFW_S_CANT_CUE } else { S_OK }
        })
    }

    unsafe fn SetSyncSource(&self, clock: *mut c_void) -> HRESULT {
        guard_hr(|| {
            // SAFETY: `clock` is null or a live IReferenceClock borrowed for the call.
            let clock = unsafe { IReferenceClock::from_raw_borrowed(&clock) }.cloned();
            self.shared.state().clock = clock.map(Agile);
            S_OK
        })
    }

    unsafe fn GetSyncSource(&self, out: *mut *mut c_void) -> HRESULT {
        guard_hr(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let clock = self.shared.state().clock.as_ref().map(|c| c.0.clone());
            // SAFETY: Ownership of one reference moves to the caller.
            unsafe { *out = clock.map_or(core::ptr::null_mut(), Interface::into_raw) };
            S_OK
        })
    }
}

impl IBaseFilterRaw_Impl for SourceFilter_Impl {
    unsafe fn EnumPins(&self, out: *mut *mut c_void) -> HRESULT {
        guard_hr(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let enumerator: IEnumPins = PinEnumerator::create(self.pin().into_iter().collect(), 0);
            // SAFETY: Ownership of one reference moves to the caller.
            unsafe { *out = enumerator.into_raw() };
            S_OK
        })
    }

    unsafe fn FindPin(&self, id: PCWSTR, out: *mut *mut c_void) -> HRESULT {
        guard_hr(|| {
            if out.is_null() || id.is_null() {
                return E_POINTER;
            }
            // SAFETY: Non-null out pointer; `id` is a NUL-terminated string.
            unsafe { *out = core::ptr::null_mut() };
            let matches = unsafe { id.to_string() }.is_ok_and(|id| id == PIN_NAME);
            match self.pin().filter(|_| matches) {
                Some(pin) => {
                    // SAFETY: Ownership of one reference moves to the caller.
                    unsafe { *out = pin.into_raw() };
                    S_OK
                }
                None => VFW_E_NOT_FOUND,
            }
        })
    }

    unsafe fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> HRESULT {
        guard_hr(|| {
            if info.is_null() {
                return E_POINTER;
            }
            let state = self.shared.state();
            let mut name = state.graph_name;
            if name[0] == 0 {
                for (slot, unit) in name.iter_mut().take(127).zip(self.shared.camera.name.encode_utf16()) {
                    *slot = unit;
                }
            }
            let graph = state.graph as *mut c_void;
            // SAFETY: The graph pointer stays valid while we are joined to it;
            // the copy handed out carries its own reference.
            let graph = unsafe { IFilterGraph::from_raw_borrowed(&graph) }.cloned();
            // SAFETY: Non-null caller out pointer; `pGraph` is caller-owned.
            unsafe {
                (*info).achName = name;
                core::ptr::write(&mut (*info).pGraph, core::mem::ManuallyDrop::new(graph));
            }
            S_OK
        })
    }

    unsafe fn JoinFilterGraph(&self, graph: *mut c_void, name: PCWSTR) -> HRESULT {
        guard_hr(|| {
            let mut state = self.shared.state();
            state.graph = graph as usize;
            state.graph_name = [0; 128];
            if !graph.is_null() && !name.is_null() {
                // SAFETY: `name` is a NUL-terminated string from the graph.
                let units = unsafe { name.as_wide() };
                let count = units.len().min(127);
                state.graph_name[..count].copy_from_slice(&units[..count]);
            }
            S_OK
        })
    }

    unsafe fn QueryVendorInfo(&self, _info: *mut PWSTR) -> HRESULT {
        E_NOTIMPL
    }
}

impl IAMFilterMiscFlags_Impl for SourceFilter_Impl {
    fn GetMiscFlags(&self) -> u32 {
        AM_FILTER_MISC_FLAGS_IS_SOURCE.0 as u32
    }
}
