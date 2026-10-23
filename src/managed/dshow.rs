//! DirectShow camera registration: COM class + video-input category instance.
//!
//! Writes, per camera, into `Software\Classes` of the selected hive:
//!
//! ```text
//! CLSID\{ID}                         (default) = friendly name
//! CLSID\{ID}\InprocServer32          (default) = source DLL, ThreadingModel = Both
//! CLSID\{VideoInputDeviceCategory}\Instance\{ID}
//!                                    CLSID, FriendlyName, FilterData
//! ```
//!
//! `FilterData` is produced by the system `IAMFilterData::CreateFilterData`,
//! not hand-serialized: REGFILTER2 v1, `MERIT_DO_NOT_USE`, one rendered=FALSE
//! output pin listing every managed video subtype. Writing the instance key
//! ourselves (instead of `IFilterMapper2::RegisterFilter`, which always
//! targets the machine hive) is what makes per-user registration possible
//! without elevation; HKCR merges `HKCU\Software\Classes` over HKLM.

use std::path::Path;

use virtualcam_protocol::PixelFormat;
use virtualcam_protocol::dshow::guids;
use windows::Win32::Media::DirectShow::{MERIT_DO_NOT_USE, REGFILTER2, REGFILTER2_0, REGFILTER2_0_0, REGFILTERPINS, REGPINTYPES};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, interface};

use super::com::{ComApartment, hresult_error};
use crate::platform::registry::{Hive, Key};
use crate::{Error, ManagedCameraId, Result};

#[interface("97F7C4D4-547B-4A5F-8332-536430AD2E4D")]
unsafe trait IAMFilterData: IUnknown {
    fn parse_filter_data(&self, data: *const u8, length: u32, filter: *mut *mut u8) -> HRESULT;
    fn create_filter_data(&self, filter: *const REGFILTER2, data: *mut *mut u8, length: *mut u32) -> HRESULT;
}

const CLASSES: &str = "Software\\Classes";

fn class_key(id: ManagedCameraId) -> String {
    format!("{CLASSES}\\CLSID\\{id}")
}

fn instance_key(id: ManagedCameraId) -> String {
    format!("{CLASSES}\\CLSID\\{}\\Instance\\{id}", ManagedCameraId::from_u128(guids::VIDEO_INPUT_DEVICE_CATEGORY))
}

/// Serializes the camera's filter registration with the system FilterMapper.
fn create_filter_data() -> Result<Vec<u8>> {
    let _apartment = ComApartment::enter()?;
    let major = GUID::from_u128(guids::MEDIATYPE_VIDEO);
    let subtypes: Vec<GUID> = PixelFormat::MANAGED.iter().filter_map(|format| format.dshow_subtype()).map(GUID::from_u128).collect();
    let types: Vec<REGPINTYPES> = subtypes
        .iter()
        .map(|subtype| REGPINTYPES {
            clsMajorType: &major,
            clsMinorType: subtype,
        })
        .collect();
    let pin = REGFILTERPINS {
        bOutput: true.into(),
        nMediaTypes: types.len() as u32,
        lpMediaType: types.as_ptr(),
        ..REGFILTERPINS::default()
    };
    let filter = REGFILTER2 {
        dwVersion: 1,
        dwMerit: MERIT_DO_NOT_USE.0 as u32,
        Anonymous: REGFILTER2_0 {
            Anonymous1: REGFILTER2_0_0 { cPins: 1, rgPins: &pin },
        },
    };

    // SAFETY: Standard in-process activation of the system filter mapper.
    let mapper: IUnknown = unsafe { CoCreateInstance(&GUID::from_u128(guids::FILTER_MAPPER2), None, CLSCTX_INPROC_SERVER) }.map_err(|error| hresult_error("CoCreateInstance(FilterMapper2)", &error))?;
    let data: IAMFilterData = mapper.cast().map_err(|error| hresult_error("QueryInterface(IAMFilterData)", &error))?;
    let mut bytes: *mut u8 = core::ptr::null_mut();
    let mut length = 0_u32;
    // SAFETY: Every pointer in `filter` refers to locals that outlive the
    // call; the output buffer is CoTaskMemAlloc'd and freed below.
    let hr = unsafe { data.create_filter_data(&filter, &mut bytes, &mut length) };
    if hr.is_err() || bytes.is_null() {
        return Err(Error::Hresult {
            operation: "IAMFilterData::CreateFilterData",
            code: hr.0,
        });
    }
    // SAFETY: CreateFilterData returned `length` initialized bytes.
    let blob = unsafe { core::slice::from_raw_parts(bytes, length as usize) }.to_vec();
    // SAFETY: Releases the mapper-allocated buffer exactly once.
    unsafe { CoTaskMemFree(Some(bytes.cast())) };
    Ok(blob)
}

/// Registers camera `id` in `hive`, rolling back every key on failure.
pub(super) fn register(hive: Hive, id: ManagedCameraId, name: &str, source: &Path) -> Result<()> {
    let filter_data = create_filter_data()?;
    let source = source.to_str().ok_or(Error::RegistrationDegraded {
        detail: "source path is not valid Unicode",
    })?;
    let result = (|| {
        let class = Key::create(hive, &class_key(id))?;
        class.set_string(None, name)?;
        let server = Key::create(hive, &format!("{}\\InprocServer32", class_key(id)))?;
        server.set_string(None, source)?;
        server.set_string(Some("ThreadingModel"), "Both")?;
        let instance = Key::create(hive, &instance_key(id))?;
        instance.set_string(Some("CLSID"), &id.to_string())?;
        instance.set_string(Some("FriendlyName"), name)?;
        instance.set_binary("FilterData", &filter_data)
    })();
    if result.is_err() {
        let _ = unregister(hive, id);
    }
    result
}

/// Removes the category instance first (so enumerators stop listing the
/// camera), then the COM class.
pub(super) fn unregister(hive: Hive, id: ManagedCameraId) -> Result<()> {
    let instance = Key::delete_tree(hive, &instance_key(id));
    let class = Key::delete_tree(hive, &class_key(id));
    instance.and(class)
}

/// Updates the friendly name shown by enumerators and COM.
pub(super) fn rename(hive: Hive, id: ManagedCameraId, name: &str) -> Result<()> {
    let instance = Key::open_writable(hive, &instance_key(id))?.ok_or(Error::RegistrationDegraded {
        detail: "DirectShow category instance is missing",
    })?;
    let class = Key::open_writable(hive, &class_key(id))?.ok_or(Error::RegistrationDegraded { detail: "DirectShow COM class is missing" })?;
    instance.set_string(Some("FriendlyName"), name)?;
    class.set_string(None, name)
}

/// Verifies that both registration halves exist and point at an existing DLL.
pub(super) fn health(hive: Hive, id: ManagedCameraId, expected_source: Option<&str>) -> core::result::Result<(), &'static str> {
    let server = Key::open(hive, &format!("{}\\InprocServer32", class_key(id))).ok().flatten().ok_or("DirectShow COM class is missing")?;
    if !Key::exists(hive, &instance_key(id)) {
        return Err("DirectShow category instance is missing");
    }
    let path = server.get_string(None).ok().flatten().ok_or("InprocServer32 path is missing")?;
    if expected_source.is_some_and(|expected| !expected.eq_ignore_ascii_case(&path)) {
        return Err("InprocServer32 path differs from the recorded source");
    }
    if !Path::new(&path).is_file() {
        return Err("DirectShow source DLL does not exist");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_filter_mapper_serializes_v1_single_output_pin() {
        let blob = create_filter_data().expect("CreateFilterData");
        // REGFILTER2 serialization begins with dwVersion=2 (the serialized
        // container version) followed by the merit.
        assert!(blob.len() > 16);
        assert_eq!(u32::from_le_bytes(blob[4..8].try_into().expect("merit")), MERIT_DO_NOT_USE.0 as u32);
    }

    #[test]
    fn keys_live_under_the_hive_classes_root() {
        let id: ManagedCameraId = "{12345678-9ABC-DEF0-1122-334455667788}".parse().expect("id");
        assert_eq!(class_key(id), "Software\\Classes\\CLSID\\{12345678-9ABC-DEF0-1122-334455667788}");
        assert_eq!(instance_key(id), "Software\\Classes\\CLSID\\{860BB310-5D01-11D0-BD3B-00A0C911CE86}\\Instance\\{12345678-9ABC-DEF0-1122-334455667788}");
    }
}
