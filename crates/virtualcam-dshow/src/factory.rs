//! `IClassFactory` for one camera CLSID.

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{BOOL, GUID, IUnknown, Interface, Ref, implement};

use crate::config::CameraConfig;
use crate::filter::SourceFilter;
use crate::{LIVE, ModuleRef, guard};

#[implement(IClassFactory)]
pub(crate) struct ClassFactory {
    camera: CameraConfig,
    locks: AtomicIsize,
    _module: ModuleRef,
}

impl ClassFactory {
    pub(crate) fn new(camera: CameraConfig) -> Self {
        Self {
            camera,
            locks: AtomicIsize::new(0),
            _module: ModuleRef::new(),
        }
    }
}

impl Drop for ClassFactory {
    fn drop(&mut self) {
        // Release server locks a client leaked on this factory.
        let leaked = self.locks.swap(0, Ordering::SeqCst);
        if leaked > 0 {
            LIVE.fetch_sub(leaked as usize, Ordering::SeqCst);
        }
    }
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, punkouter: Ref<IUnknown>, riid: *const GUID, ppvobject: *mut *mut c_void) -> windows_core::Result<()> {
        guard(|| {
            if ppvobject.is_null() || riid.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: Non-null caller out pointer.
            unsafe { *ppvobject = core::ptr::null_mut() };
            if punkouter.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            let filter = SourceFilter::create(self.camera.clone());
            // SAFETY: Standard QueryInterface into the caller's out pointer.
            unsafe { filter.query(riid, ppvobject) }.ok()
        })
    }

    fn LockServer(&self, flock: BOOL) -> windows_core::Result<()> {
        if flock.as_bool() {
            self.locks.fetch_add(1, Ordering::SeqCst);
            LIVE.fetch_add(1, Ordering::SeqCst);
        } else if self.locks.fetch_sub(1, Ordering::SeqCst) > 0 {
            LIVE.fetch_sub(1, Ordering::SeqCst);
        } else {
            self.locks.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}
