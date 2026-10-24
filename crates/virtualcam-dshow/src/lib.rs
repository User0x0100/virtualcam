//! Pure Rust DirectShow source filter backing `virtualcam` managed cameras.
//!
//! One DLL serves every registered DirectShow camera: the requested CLSID is
//! the camera id, validated against the camera records written by the
//! manager. The filter exposes one capture output pin and advertises the
//! current format (the live producer's, else the registered idle default,
//! else the user's last one) plus every other registered pixel format at the
//! same size and rate. One graph at a time may connect; it publishes its
//! negotiated format so the producer can switch pixel format. Frames are
//! pushed as the producer publishes them from its transport v2 ring — black
//! while no producer (or one in another format) is attached.
//!
//! Loader-lock discipline: there is no `DllMain` work at all; everything is
//! initialized lazily through normal COM activation. Every exported function
//! and COM method runs inside [`guard`], so a Rust panic becomes
//! `E_UNEXPECTED` instead of unwinding into the host process.

#![cfg(windows)]
#![forbid(unsafe_op_in_unsafe_fn)]
#![allow(linker_messages)]

mod config;
mod consumer;
mod factory;
mod filter;
mod media_type;
mod pin;
mod stream;
mod transport;
mod win;

use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;
use windows_core::{GUID, HRESULT, Interface};

/// Live COM objects, server locks, and worker threads; non-zero keeps the DLL loaded.
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

/// [`guard`] for methods that return a raw `HRESULT`.
pub(crate) fn guard_hr(f: impl FnOnce() -> HRESULT) -> HRESULT {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(E_UNEXPECTED)
}

/// COM entry point: returns the class factory of a registered camera.
///
/// # Safety
///
/// Called by COM with valid `rclsid`/`riid` pointers and a writable `ppv`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    guard_hr(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: COM passes a writable out pointer.
        unsafe { *ppv = core::ptr::null_mut() };
        if rclsid.is_null() || riid.is_null() {
            return E_POINTER;
        }
        // SAFETY: Non-null pointers supplied by COM.
        let (clsid, iid) = unsafe { (*rclsid, *riid) };
        let Some(camera) = config::CameraConfig::load(virtualcam_protocol::CameraId::from_u128(clsid.to_u128())) else {
            return CLASS_E_CLASSNOTAVAILABLE;
        };
        let factory: IClassFactory = factory::ClassFactory::new(camera).into();
        // SAFETY: Standard QueryInterface into the caller's out pointer.
        unsafe { factory.query(&iid, ppv) }
    })
}

/// COM entry point: reports whether no object, lock, or worker is alive.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE.load(Ordering::SeqCst) == 0 { S_OK } else { S_FALSE }
}

#[cfg(test)]
mod tests {
    use virtualcam_protocol::registry::{BACKEND_DIRECTSHOW, CAMERAS_KEY, values};
    use virtualcam_protocol::{FrameRate, PixelFormat, VideoFormat};
    use windows::Win32::Media::DirectShow::{IAMStreamConfig, IBaseFilter, IPin, State_Paused, VFW_S_CANT_CUE, VIDEO_STREAM_CONFIG_CAPS};
    use windows::Win32::Media::KernelStreaming::IKsPropertySet;
    use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
    use windows_core::PCWSTR;

    use super::*;
    use crate::media_type::{MediaType, parse};
    use crate::win::wide;

    struct Record(String);

    impl Drop for Record {
        fn drop(&mut self) {
            let path = wide(&self.0);
            // SAFETY: Removes only this test's HKCU record.
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        }
    }

    fn write_record(id: virtualcam_protocol::CameraId, format: VideoFormat) -> Record {
        use windows::Win32::System::Registry::{HKEY, KEY_WRITE, REG_BINARY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
        let path = format!(r"{CAMERAS_KEY}\{id}");
        let wide_path = wide(&path);
        let mut key = HKEY::default();
        // SAFETY: Test-only HKCU key creation with valid out pointer.
        unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, PCWSTR(wide_path.as_ptr()), None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None) }
            .ok()
            .expect("create record");
        let set = |name: &str, kind, data: &[u8]| {
            let name = wide(name);
            // SAFETY: Open key, valid buffers.
            unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, kind, Some(data)) }.ok().expect("set value");
        };
        set(values::BACKEND, REG_DWORD, &BACKEND_DIRECTSHOW.to_le_bytes());
        let name: Vec<u8> = wide("Unit Test Cam").iter().flat_map(|u| u.to_le_bytes()).collect();
        set(values::NAME, REG_SZ, &name);
        set(values::FORMAT, REG_BINARY, &format.to_wire().expect("wire").to_bytes());
        let formats = virtualcam_protocol::registry::encode_pixel_formats(&[PixelFormat::Nv12, PixelFormat::Uyvy, PixelFormat::Rgb24]);
        set(values::PIXEL_FORMATS, REG_BINARY, &formats);
        // SAFETY: Closes the key opened above.
        let _ = unsafe { RegCloseKey(key) };
        Record(path)
    }

    #[test]
    fn filter_exposes_capture_pin_caps_and_live_source_state() {
        // SAFETY: Test thread joins the MTA.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let id = virtualcam_protocol::CameraId::from_u128(0x7E57_0000_0000_4000_8000_0000_0000_0000 | u128::from(std::process::id()));
        let format = VideoFormat::new(PixelFormat::Uyvy, 352, 288, FrameRate::fps(25));
        let _record = write_record(id, format);
        let clsid = GUID::from_u128(id.to_u128());

        {
            let mut factory: *mut c_void = core::ptr::null_mut();
            // SAFETY: Valid pointers for the export.
            assert_eq!(unsafe { DllGetClassObject(&clsid, &IClassFactory::IID, &mut factory) }, S_OK);
            // SAFETY: The export returned one owned IClassFactory reference.
            let factory = unsafe { IClassFactory::from_raw(factory) };
            // SAFETY: Valid factory.
            let filter: IBaseFilter = unsafe { factory.CreateInstance(None) }.expect("create filter");
            // SAFETY: Valid filter; the pin name is NUL-terminated.
            let pin: IPin = unsafe { filter.FindPin(windows_core::w!("Capture")) }.expect("capture pin");
            assert_eq!(unsafe { filter.GetClassID() }.expect("clsid"), clsid);

            let ks: IKsPropertySet = pin.cast().expect("IKsPropertySet");
            let mut category = GUID::zeroed();
            let mut returned = 0_u32;
            let set = GUID::from_u128(virtualcam_protocol::dshow::guids::AMPROPSETID_PIN);
            // SAFETY: Valid buffers for one GUID.
            unsafe { ks.Get(&set, 0, core::ptr::null(), 0, (&raw mut category).cast(), 16, &mut returned) }.expect("pin category");
            assert_eq!(category, GUID::from_u128(virtualcam_protocol::dshow::guids::PIN_CATEGORY_CAPTURE));

            // Idle advertises the registered default; GetStreamCaps must not deadlock.
            let config: IAMStreamConfig = pin.cast().expect("IAMStreamConfig");
            let mut count = 0;
            let mut size = 0;
            // SAFETY: Valid out pointers.
            unsafe { config.GetNumberOfCapabilities(&mut count, &mut size) }.expect("caps count");
            assert_eq!((count, size as usize), (3, size_of::<VIDEO_STREAM_CONFIG_CAPS>()));
            let mut mt: *mut AM_MEDIA_TYPE = core::ptr::null_mut();
            let mut caps = VIDEO_STREAM_CONFIG_CAPS::default();
            // SAFETY: Valid out pointers; `caps` has the reported size.
            unsafe { config.GetStreamCaps(0, &mut mt, (&raw mut caps).cast()) }.expect("stream caps");
            // SAFETY: `mt` is a valid heap media type we now own.
            let parsed = unsafe { parse(mt) }.expect("parse caps type");
            assert!(parsed.matches(&format));
            assert_eq!(caps.MinFrameInterval, 400_000);
            // SAFETY: Frees the caller-owned type (format block, then struct).
            unsafe {
                drop(MediaType(core::ptr::read(mt)));
                CoTaskMemFree(Some(mt.cast()));
            }
            // The other registered formats follow at the same size and rate.
            for (index, expected) in [(1, PixelFormat::Nv12), (2, PixelFormat::Rgb24)] {
                let mut mt: *mut AM_MEDIA_TYPE = core::ptr::null_mut();
                // SAFETY: Valid out pointers; `caps` has the reported size.
                unsafe { config.GetStreamCaps(index, &mut mt, (&raw mut caps).cast()) }.expect("stream caps");
                // SAFETY: `mt` is a valid heap media type we now own.
                let parsed = unsafe { parse(mt) }.expect("parse caps type");
                assert_eq!((parsed.pixel_format, parsed.width, parsed.height), (expected, 352, 288));
                // SAFETY: Frees the caller-owned type (format block, then struct).
                unsafe {
                    drop(MediaType(core::ptr::read(mt)));
                    CoTaskMemFree(Some(mt.cast()));
                }
            }

            // A live source reports VFW_S_CANT_CUE while paused.
            // SAFETY: Valid filter.
            unsafe { filter.Pause() }.expect("pause");
            let mut state = Default::default();
            // SAFETY: Raw vtable call to observe the success code.
            let hr = unsafe { (Interface::vtable(&filter).base__.GetState)(filter.as_raw(), 0, &mut state) };
            assert_eq!((hr, state), (VFW_S_CANT_CUE, State_Paused));
            // SAFETY: Valid filter.
            unsafe { filter.Stop() }.expect("stop");
            assert_eq!(DllCanUnloadNow(), S_FALSE);
        }
        assert_eq!(DllCanUnloadNow(), S_OK, "every object released");

        let mut unknown: *mut c_void = core::ptr::null_mut();
        let missing = GUID::from_u128(0x7E57_FFFF_0000_4000_8000_0000_0000_0001);
        // SAFETY: Valid pointers for the export.
        assert_eq!(unsafe { DllGetClassObject(&missing, &IClassFactory::IID, &mut unknown) }, CLASS_E_CLASSNOTAVAILABLE);
        assert!(unknown.is_null());
    }
}
