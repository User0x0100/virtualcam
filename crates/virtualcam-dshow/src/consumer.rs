//! The single-consumer lease and the consumer-to-producer format request.
//!
//! Only one graph at a time may connect a camera's output pin. The holder
//! owns `Local\VirtualCamRs2-{ID}-Consumer` (existence = lease) and is the
//! only writer of the request mapping, where it publishes the format its
//! graph connected with so the producer can switch to that pixel format.

use core::ffi::c_void;

use virtualcam_protocol::dshow::{DshowNames, DshowRequest, REQUEST_STATE_CONNECTED, REQUEST_STATE_NONE};
use virtualcam_protocol::{VideoFormat, WireVideoFormat};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId};
use windows_core::PCWSTR;

use crate::win::Handle;

/// Why the lease could not be taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseError {
    /// Another graph holds it.
    Busy,
    /// The lease or request object could not be created.
    Unavailable,
}

pub(crate) struct ConsumerLease {
    // Field order is drop order: unmap, then the mapping, then the lease.
    view: *mut DshowRequest,
    _mapping: Handle,
    _lease: Handle,
}

// SAFETY: The view is plain process memory and the handles have no thread
// affinity; all writes go through `&mut self` or `Drop`.
unsafe impl Send for ConsumerLease {}

impl ConsumerLease {
    pub(crate) fn acquire(names: &DshowNames) -> Result<Self, LeaseError> {
        // SAFETY: NUL-terminated name, default security; manual-reset (never signaled).
        let lease = Handle(unsafe { CreateEventW(None, true, false, PCWSTR(names.consumer.as_ptr())) }.map_err(|_| LeaseError::Unavailable)?);
        // SAFETY: Reads the calling thread's last error, set by CreateEventW.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(LeaseError::Busy);
        }
        // SAFETY: Page-file-backed mapping; a producer may have created it first.
        let mapping = Handle(unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, DshowRequest::SIZE as u32, PCWSTR(names.request.as_ptr())) }.map_err(|_| LeaseError::Unavailable)?);
        // SAFETY: Whole-mapping view of the live section.
        let view = unsafe { MapViewOfFile(mapping.0, FILE_MAP_ALL_ACCESS, 0, 0, DshowRequest::SIZE) };
        let view = view.Value.cast::<DshowRequest>();
        if view.is_null() || view.align_offset(64) != 0 {
            return Err(LeaseError::Unavailable);
        }
        Ok(Self { view, _mapping: mapping, _lease: lease })
    }

    /// Publishes the connected graph's format.
    pub(crate) fn publish(&mut self, format: &VideoFormat) {
        if let Ok(wire) = format.to_wire() {
            self.write(REQUEST_STATE_CONNECTED, wire);
        }
    }

    fn write(&mut self, state: u32, format: WireVideoFormat) {
        // SAFETY: Trivial process query.
        let record = DshowRequest::new(state, format, unsafe { GetCurrentProcessId() });
        // SAFETY: Live, aligned view; the lease makes us the only writer.
        unsafe { DshowRequest::publish(self.view, &record) };
    }
}

impl Drop for ConsumerLease {
    fn drop(&mut self) {
        self.write(REQUEST_STATE_NONE, WireVideoFormat::default());
        // SAFETY: Base address returned by MapViewOfFile, unmapped once.
        let _ = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view.cast::<c_void>() }) };
    }
}
