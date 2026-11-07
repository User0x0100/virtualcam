//! Pure Rust Media Foundation custom media source for `virtualcam` cameras.
//!
//! Frame Server (Local Service, session 0) activates the fixed
//! [`SOURCE_CLSID`](virtualcam_protocol::mf::SOURCE_CLSID) once per virtual
//! camera and copies that camera's vendor attributes into our
//! `IMFActivate`. The source advertises the immutable native mode from
//! those attributes plus the conversions it performs itself (NV12/I420 to
//! YUY2, see `virtualcam_protocol::convert`), produces samples synchronously on `RequestSample`
//! (black until a producer attaches), and lazily authenticates producers on a
//! named pipe before handing them a freshly created, DACL-protected ring.
//!
//! No work happens in `DllMain`; every export and COM method is
//! panic-guarded; activate and source aggregate the free-threaded marshaler.

#![cfg(windows)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(linker_messages)]

mod activate;
mod config;
mod factory;
mod ftm;
mod rendezvous;
mod security;
mod source;
mod stream;

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;
use windows_core::{GUID, HRESULT, Interface};

/// Live COM objects, server locks, and worker threads.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// RAII contribution to [`LIVE`].
pub(crate) struct ModuleRef(());

impl ModuleRef {
    pub(crate) fn new() -> Self {
        LIVE.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Runs `f`, converting a panic into `E_UNEXPECTED`.
pub(crate) fn guard<T>(f: impl FnOnce() -> windows_core::Result<T>) -> windows_core::Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// COM entry point for the fixed source CLSID.
///
/// # Safety
///
/// Called by COM with valid `rclsid`/`riid` pointers and a writable `ppv`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM passes a writable out pointer.
        unsafe { *ppv = core::ptr::null_mut() };
        if rclsid.is_null() || riid.is_null() {
            return E_POINTER;
        }
        // SAFETY: Non-null pointers supplied by COM.
        if unsafe { *rclsid }.to_u128() != virtualcam_protocol::mf::SOURCE_CLSID.to_u128() {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = factory::ClassFactory::new().into();
        // SAFETY: Standard QueryInterface into the caller's out pointer.
        unsafe { factory.query(riid, ppv) }
    }))
    .unwrap_or(E_UNEXPECTED)
}

/// COM entry point: reports whether no object, lock, or worker is alive.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::SeqCst) == 0 { S_OK } else { S_FALSE }
}

#[cfg(test)]
mod tests;
