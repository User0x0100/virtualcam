//! `IMFActivate` for the fixed source CLSID.
//!
//! Frame Server copies the virtual camera's attributes into this object's
//! attribute store, then calls `ActivateObject`. Repeated activations of one
//! activate object return the same source until it is shut down, so one
//! activation never owns two unrelated transports.

use core::ffi::c_void;
use std::sync::Mutex;

use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::{IMFActivate, IMFActivate_Impl, IMFAttributes, IMFAttributes_Impl, MF_ATTRIBUTE_TYPE, MF_ATTRIBUTES_MATCH_TYPE, MFCreateAttributes};
use windows::Win32::System::Com::Marshal::IMarshal;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl};
use windows_core::{BOOL, ComObject, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::config::SourceConfig;
use crate::ftm::{Ftm, forward_marshal};
use crate::source::MediaSource;
use crate::{ModuleRef, guard};

#[implement(IMFActivate, IMarshal, IAgileObject)]
pub(crate) struct SourceActivate {
    store: IMFAttributes,
    source: Mutex<Option<ComObject<MediaSource>>>,
    ftm: Ftm,
    _module: ModuleRef,
}

impl SourceActivate {
    pub(crate) fn create() -> windows_core::Result<IUnknown> {
        let mut store = None;
        // SAFETY: Out pointer receives a new attribute store.
        unsafe { MFCreateAttributes(&mut store, 16) }?;
        let activate = ComObject::new(Self {
            store: store.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY))?,
            source: Mutex::new(None),
            ftm: Ftm::default(),
            _module: ModuleRef::new(),
        });
        let unknown: IUnknown = activate.to_interface();
        activate.ftm.attach(&unknown)?;
        Ok(unknown)
    }
}

forward_marshal!(SourceActivate_Impl);

impl IAgileObject_Impl for SourceActivate_Impl {}

impl IMFActivate_Impl for SourceActivate_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        guard(|| {
            let mut cached = self.source.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if cached.as_ref().is_none_or(|source| source.is_shut_down()) {
                let config = SourceConfig::from_attributes(&self.store)?;
                *cached = Some(MediaSource::create(config)?);
            }
            let source = cached
                .as_ref()
                .map(|source| source.to_interface::<IUnknown>())
                .ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_UNEXPECTED))?;
            // SAFETY: Standard QueryInterface into the caller's out pointer.
            unsafe { source.query(riid, ppv) }.ok()
        })
    }

    /// Frame Server custom sources return `E_NOTIMPL` here by contract.
    fn ShutdownObject(&self) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    /// Frame Server custom sources return `E_NOTIMPL` here by contract.
    fn DetachObject(&self) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

/// Forwards one `IMFAttributes` method through the store's vtable, keeping
/// every argument and the exact HRESULT.
macro_rules! forward {
    ($self:ident.$method:ident($($arg:expr),*)) => {{
        let store = &$self.store;
        // SAFETY: Arguments come straight from our caller's COM call.
        unsafe { (Interface::vtable(store).$method)(store.as_raw() $(, $arg)*) }.ok()
    }};
}

fn raw<T: Interface>(value: Ref<T>) -> *mut c_void {
    value.as_ref().map_or(core::ptr::null_mut(), Interface::as_raw)
}

impl IMFAttributes_Impl for SourceActivate_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> windows_core::Result<()> {
        forward!(self.GetItem(key, value))
    }

    fn GetItemType(&self, key: *const GUID) -> windows_core::Result<MF_ATTRIBUTE_TYPE> {
        let mut out = MF_ATTRIBUTE_TYPE::default();
        forward!(self.GetItemType(key, &mut out)).map(|()| out)
    }

    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows_core::Result<BOOL> {
        let mut out = BOOL::default();
        forward!(self.CompareItem(key, value, &mut out)).map(|()| out)
    }

    fn Compare(&self, theirs: Ref<IMFAttributes>, match_type: MF_ATTRIBUTES_MATCH_TYPE) -> windows_core::Result<BOOL> {
        let mut out = BOOL::default();
        forward!(self.Compare(raw(theirs), match_type, &mut out)).map(|()| out)
    }

    fn GetUINT32(&self, key: *const GUID) -> windows_core::Result<u32> {
        let mut out = 0;
        forward!(self.GetUINT32(key, &mut out)).map(|()| out)
    }

    fn GetUINT64(&self, key: *const GUID) -> windows_core::Result<u64> {
        let mut out = 0;
        forward!(self.GetUINT64(key, &mut out)).map(|()| out)
    }

    fn GetDouble(&self, key: *const GUID) -> windows_core::Result<f64> {
        let mut out = 0.0;
        forward!(self.GetDouble(key, &mut out)).map(|()| out)
    }

    fn GetGUID(&self, key: *const GUID) -> windows_core::Result<GUID> {
        let mut out = GUID::zeroed();
        forward!(self.GetGUID(key, &mut out)).map(|()| out)
    }

    fn GetStringLength(&self, key: *const GUID) -> windows_core::Result<u32> {
        let mut out = 0;
        forward!(self.GetStringLength(key, &mut out)).map(|()| out)
    }

    fn GetString(&self, key: *const GUID, value: PWSTR, capacity: u32, length: *mut u32) -> windows_core::Result<()> {
        forward!(self.GetString(key, value, capacity, length))
    }

    fn GetAllocatedString(&self, key: *const GUID, value: *mut PWSTR, length: *mut u32) -> windows_core::Result<()> {
        forward!(self.GetAllocatedString(key, value, length))
    }

    fn GetBlobSize(&self, key: *const GUID) -> windows_core::Result<u32> {
        let mut out = 0;
        forward!(self.GetBlobSize(key, &mut out)).map(|()| out)
    }

    fn GetBlob(&self, key: *const GUID, buffer: *mut u8, capacity: u32, size: *mut u32) -> windows_core::Result<()> {
        forward!(self.GetBlob(key, buffer, capacity, size))
    }

    fn GetAllocatedBlob(&self, key: *const GUID, buffer: *mut *mut u8, size: *mut u32) -> windows_core::Result<()> {
        forward!(self.GetAllocatedBlob(key, buffer, size))
    }

    fn GetUnknown(&self, key: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        forward!(self.GetUnknown(key, riid, ppv))
    }

    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows_core::Result<()> {
        forward!(self.SetItem(key, value))
    }

    fn DeleteItem(&self, key: *const GUID) -> windows_core::Result<()> {
        forward!(self.DeleteItem(key))
    }

    fn DeleteAllItems(&self) -> windows_core::Result<()> {
        forward!(self.DeleteAllItems())
    }

    fn SetUINT32(&self, key: *const GUID, value: u32) -> windows_core::Result<()> {
        forward!(self.SetUINT32(key, value))
    }

    fn SetUINT64(&self, key: *const GUID, value: u64) -> windows_core::Result<()> {
        forward!(self.SetUINT64(key, value))
    }

    fn SetDouble(&self, key: *const GUID, value: f64) -> windows_core::Result<()> {
        forward!(self.SetDouble(key, value))
    }

    fn SetGUID(&self, key: *const GUID, value: *const GUID) -> windows_core::Result<()> {
        forward!(self.SetGUID(key, value))
    }

    fn SetString(&self, key: *const GUID, value: &PCWSTR) -> windows_core::Result<()> {
        forward!(self.SetString(key, *value))
    }

    fn SetBlob(&self, key: *const GUID, buffer: *const u8, size: u32) -> windows_core::Result<()> {
        forward!(self.SetBlob(key, buffer, size))
    }

    fn SetUnknown(&self, key: *const GUID, value: Ref<IUnknown>) -> windows_core::Result<()> {
        forward!(self.SetUnknown(key, raw(value)))
    }

    fn LockStore(&self) -> windows_core::Result<()> {
        forward!(self.LockStore())
    }

    fn UnlockStore(&self) -> windows_core::Result<()> {
        forward!(self.UnlockStore())
    }

    fn GetCount(&self) -> windows_core::Result<u32> {
        let mut out = 0;
        forward!(self.GetCount(&mut out)).map(|()| out)
    }

    fn GetItemByIndex(&self, index: u32, key: *mut GUID, value: *mut PROPVARIANT) -> windows_core::Result<()> {
        forward!(self.GetItemByIndex(index, key, value))
    }

    fn CopyAllItems(&self, destination: Ref<IMFAttributes>) -> windows_core::Result<()> {
        forward!(self.CopyAllItems(raw(destination)))
    }
}
