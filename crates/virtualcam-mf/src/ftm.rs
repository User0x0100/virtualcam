//! Free-threaded marshaler aggregation.
//!
//! `ThreadingModel=Both` is only a registration hint; Frame Server and
//! Media Foundation additionally require the activate and source to be agile.
//! Each object aggregates the system FTM and forwards `IMarshal` to it.

use std::sync::OnceLock;

use windows::Win32::System::Com::CoCreateFreeThreadedMarshaler;
use windows::Win32::System::Com::Marshal::IMarshal;
use windows_core::{IUnknown, Interface};

/// The aggregated FTM, created right after the outer object.
#[derive(Default)]
pub(crate) struct Ftm(OnceLock<IMarshal>);

impl Ftm {
    /// Aggregates the FTM into `outer`. The FTM does not reference-count its
    /// outer object, so holding it creates no cycle.
    pub(crate) fn attach(&self, outer: &IUnknown) -> windows_core::Result<()> {
        // SAFETY: `outer` is the controlling unknown of a live object.
        let inner = unsafe { CoCreateFreeThreadedMarshaler(outer) }?;
        let _ = self.0.set(inner.cast()?);
        Ok(())
    }

    pub(crate) fn get(&self) -> windows_core::Result<&IMarshal> {
        self.0.get().ok_or_else(|| windows::Win32::Foundation::E_NOINTERFACE.into())
    }
}

/// Implements `IMarshal_Impl` for `$outer` by forwarding every call
/// unchanged through the FTM's vtable to its `ftm` field.
macro_rules! forward_marshal {
    ($outer:ty) => {
        impl windows::Win32::System::Com::Marshal::IMarshal_Impl for $outer {
            fn GetUnmarshalClass(&self, riid: *const windows_core::GUID, pv: *const core::ffi::c_void, context: u32, pvcontext: *const core::ffi::c_void, flags: u32) -> windows_core::Result<windows_core::GUID> {
                let ftm = self.ftm.get()?;
                let mut out = windows_core::GUID::zeroed();
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).GetUnmarshalClass)(windows_core::Interface::as_raw(ftm), riid, pv, context, pvcontext, flags, &mut out) }.map(|| out)
            }

            fn GetMarshalSizeMax(&self, riid: *const windows_core::GUID, pv: *const core::ffi::c_void, context: u32, pvcontext: *const core::ffi::c_void, flags: u32) -> windows_core::Result<u32> {
                let ftm = self.ftm.get()?;
                let mut out = 0;
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).GetMarshalSizeMax)(windows_core::Interface::as_raw(ftm), riid, pv, context, pvcontext, flags, &mut out) }.map(|| out)
            }

            fn MarshalInterface(
                &self,
                stream: windows_core::Ref<windows::Win32::System::Com::IStream>,
                riid: *const windows_core::GUID,
                pv: *const core::ffi::c_void,
                context: u32,
                pvcontext: *const core::ffi::c_void,
                flags: u32,
            ) -> windows_core::Result<()> {
                let ftm = self.ftm.get()?;
                let stream = stream.as_ref().map_or(core::ptr::null_mut(), windows_core::Interface::as_raw);
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).MarshalInterface)(windows_core::Interface::as_raw(ftm), stream, riid, pv, context, pvcontext, flags) }.ok()
            }

            fn UnmarshalInterface(&self, stream: windows_core::Ref<windows::Win32::System::Com::IStream>, riid: *const windows_core::GUID, ppv: *mut *mut core::ffi::c_void) -> windows_core::Result<()> {
                let ftm = self.ftm.get()?;
                let stream = stream.as_ref().map_or(core::ptr::null_mut(), windows_core::Interface::as_raw);
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).UnmarshalInterface)(windows_core::Interface::as_raw(ftm), stream, riid, ppv) }.ok()
            }

            fn ReleaseMarshalData(&self, stream: windows_core::Ref<windows::Win32::System::Com::IStream>) -> windows_core::Result<()> {
                let ftm = self.ftm.get()?;
                let stream = stream.as_ref().map_or(core::ptr::null_mut(), windows_core::Interface::as_raw);
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).ReleaseMarshalData)(windows_core::Interface::as_raw(ftm), stream) }.ok()
            }

            fn DisconnectObject(&self, reserved: u32) -> windows_core::Result<()> {
                let ftm = self.ftm.get()?;
                // SAFETY: Arguments are forwarded unchanged to the FTM.
                unsafe { (windows_core::Interface::vtable(ftm).DisconnectObject)(windows_core::Interface::as_raw(ftm), reserved) }.ok()
            }
        }
    };
}

pub(crate) use forward_marshal;
