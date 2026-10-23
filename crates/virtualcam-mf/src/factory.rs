//! `IClassFactory` for the fixed source CLSID.

use core::ffi::c_void;

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_POINTER};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::{BOOL, GUID, IUnknown, Interface, Ref, implement};

use crate::activate::SourceActivate;
use crate::{LIVE, ModuleRef, guard};

#[implement(IClassFactory)]
pub(crate) struct ClassFactory {
    _module: ModuleRef,
}

impl ClassFactory {
    pub(crate) fn new() -> Self {
        Self { _module: ModuleRef::new() }
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
            let activate = SourceActivate::create()?;
            // SAFETY: Standard QueryInterface into the caller's out pointer.
            unsafe { activate.query(riid, ppvobject) }.ok()
        })
    }

    fn LockServer(&self, flock: BOOL) -> windows_core::Result<()> {
        if flock.as_bool() {
            LIVE.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        } else {
            LIVE.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
}
