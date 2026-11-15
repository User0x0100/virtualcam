//! Media Foundation camera management.
//!
//! Every camera is created with `MFCreateVirtualCamera(SoftwareCameraSource,
//! System lifetime, CurrentUser access, name, fixed source CLSID)` and
//! configured through vendor attributes (camera id, rendezvous id, owner SID
//! assertion, protocol version, default format, declared pixel formats). The
//! same tuple reopens the same system camera, which is how unregister and
//! rename find it again. The manager never needs elevation for cameras;
//! installing the shared source DLL machine-wide (once) does.

use std::path::{Path, PathBuf};

use virtualcam_protocol::mf::{ATTR_CAMERA_ID, ATTR_DEFAULT_FORMAT, ATTR_EXPECTED_OWNER_SID, ATTR_PIXEL_FORMATS, ATTR_PROTOCOL_VERSION, ATTR_RENDEZVOUS_ID, PROTOCOL_VERSION, SOURCE_CLSID};
use virtualcam_protocol::registry::encode_pixel_formats;
use virtualcam_protocol::{PixelFormat, ValidatedFormat, VideoFormat};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFVirtualCamera, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_VERSION, MFCreateVirtualCamera, MFSTARTUP_LITE, MFShutdown, MFStartup, MFVirtualCameraAccess_CurrentUser,
    MFVirtualCameraLifetime_System, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows_core::{GUID, HSTRING, Interface, PWSTR};

use super::com::{ComApartment, hresult_error};
use super::metadata::CameraRecord;
use super::{AppId, ManagedBackend, MediaFoundationRegistration};
use crate::platform::registry::{Hive, Key};
use crate::platform::windows::{current_user_sid, random_bytes};
use crate::{CameraType, Error, ManagedCameraId, Result, VirtualCamera};

fn source_class_key() -> String {
    format!("Software\\Classes\\CLSID\\{SOURCE_CLSID}")
}

/// The installed source DLL, if the machine-wide COM class exists.
pub(crate) fn installed_source() -> Option<PathBuf> {
    let key = Key::open(Hive::LocalMachine, &format!("{}\\InprocServer32", source_class_key())).ok()??;
    key.get_string(None).ok()?.map(PathBuf::from).filter(|path| path.is_file())
}

/// Registers the shared source DLL machine-wide (requires elevation).
///
/// Frame Server loads it as Local Service, so the file must live where that
/// account can read it (e.g. under Program Files), not in a user profile.
pub(crate) fn install_source(path: &Path) -> Result<()> {
    let canonical = std::fs::canonicalize(path).map_err(|_| Error::BackendUnavailable(CameraType::ManagedMediaFoundation))?;
    let text = canonical.to_str().ok_or(Error::RegistrationDegraded {
        detail: "source path is not valid Unicode",
    })?;
    let plain = text.strip_prefix(r"\\?\").unwrap_or(text);
    if plain.starts_with(r"\\") || plain.starts_with("UNC\\") {
        return Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation));
    }
    let result = (|| {
        Key::create(Hive::LocalMachine, &source_class_key())?.set_string(None, "virtualcam Media Foundation source")?;
        let server = Key::create(Hive::LocalMachine, &format!("{}\\InprocServer32", source_class_key()))?;
        server.set_string(None, plain)?;
        server.set_string(Some("ThreadingModel"), "Both")
    })();
    if result.is_err() {
        let _ = Key::delete_tree(Hive::LocalMachine, &source_class_key());
    }
    result
}

/// Removes the machine-wide source registration.
///
/// Fails safe: refuses while this user still has Media Foundation cameras.
/// Other users' cameras cannot be enumerated from here, so the caller must
/// affirm `other_users_cleaned` after cleaning them (e.g. per-user uninstall
/// steps); otherwise the source stays installed.
pub(crate) fn uninstall_source(other_users_cleaned: bool) -> Result<()> {
    let ours = CameraRecord::list(Hive::CurrentUser)?.into_iter().any(|record| record.backend == ManagedBackend::MediaFoundation);
    if ours || !other_users_cleaned {
        return Err(Error::RegistrationDegraded {
            detail: "Media Foundation cameras may still reference the source",
        });
    }
    Key::delete_tree(Hive::LocalMachine, &source_class_key())
}

/// `MFStartup`/`MFShutdown` scope (plus a COM apartment).
struct MfScope {
    _apartment: ComApartment,
}

impl MfScope {
    fn enter() -> Result<Self> {
        let apartment = ComApartment::enter()?;
        // SAFETY: Lightweight MF initialization, balanced in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(|error| hresult_error("MFStartup", &error))?;
        Ok(Self { _apartment: apartment })
    }
}

impl Drop for MfScope {
    fn drop(&mut self) {
        // SAFETY: Balances the successful MFStartup.
        let _ = unsafe { MFShutdown() };
    }
}

/// Writes the vendor attributes the source reads at activation.
pub(crate) fn configure_attributes(attributes: &IMFAttributes, id: ManagedCameraId, rendezvous_id: &str, owner_sid: &str, default_format: &VideoFormat, pixel_formats: &[PixelFormat]) -> Result<()> {
    let key = |id: ManagedCameraId| GUID::from_u128(id.to_u128());
    let wire = default_format.to_wire()?.to_bytes();
    let formats = encode_pixel_formats(pixel_formats);
    // SAFETY: Plain attribute writes on a live store.
    let result: windows_core::Result<()> = unsafe {
        (|| {
            attributes.SetString(&key(ATTR_CAMERA_ID), &HSTRING::from(id.to_string()))?;
            attributes.SetString(&key(ATTR_RENDEZVOUS_ID), &HSTRING::from(rendezvous_id))?;
            attributes.SetString(&key(ATTR_EXPECTED_OWNER_SID), &HSTRING::from(owner_sid))?;
            attributes.SetUINT32(&key(ATTR_PROTOCOL_VERSION), PROTOCOL_VERSION)?;
            attributes.SetBlob(&key(ATTR_DEFAULT_FORMAT), &wire)?;
            attributes.SetBlob(&key(ATTR_PIXEL_FORMATS), &formats)
        })()
    };
    result.map_err(|error| hresult_error("IMFAttributes::Set*(vendor attributes)", &error))
}

/// Creates (or reopens, for an identical tuple) the system camera object.
fn virtual_camera(name: &str) -> Result<IMFVirtualCamera> {
    // SAFETY: Valid strings; the result is an owned interface.
    unsafe {
        MFCreateVirtualCamera(
            MFVirtualCameraType_SoftwareCameraSource,
            MFVirtualCameraLifetime_System,
            MFVirtualCameraAccess_CurrentUser,
            &HSTRING::from(name),
            &HSTRING::from(SOURCE_CLSID.to_string()),
            None,
        )
    }
    .map_err(|error| hresult_error("MFCreateVirtualCamera", &error))
}

fn symbolic_link(camera: &IMFVirtualCamera) -> Option<String> {
    let mut value = PWSTR::null();
    let mut length = 0_u32;
    // SAFETY: Live camera; the string is CoTaskMemFree'd below.
    unsafe { camera.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, &mut value, &mut length) }.ok()?;
    // SAFETY: NUL-terminated allocation from the call above.
    let text = unsafe { value.to_string() }.ok();
    // SAFETY: Allocated by GetAllocatedString.
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    text
}

/// Creates and starts a System-lifetime camera, returning its symbolic link.
/// On failure after creation the camera is removed again.
fn create_camera(name: &str, id: ManagedCameraId, rendezvous_id: &str, owner_sid: &str, default_format: &VideoFormat, pixel_formats: &[PixelFormat]) -> Result<Option<String>> {
    let camera = virtual_camera(name)?;
    let result = (|| {
        configure_attributes(
            &camera.cast().map_err(|error| hresult_error("IMFVirtualCamera::QueryInterface(IMFAttributes)", &error))?,
            id,
            rendezvous_id,
            owner_sid,
            default_format,
            pixel_formats,
        )?;
        // SAFETY: Live camera; synchronous start (no callback).
        unsafe { camera.Start(None) }.map_err(|error| hresult_error("IMFVirtualCamera::Start", &error))?;
        Ok(symbolic_link(&camera))
    })();
    if result.is_err() {
        // SAFETY: Live camera; removes the partially created device.
        let _ = unsafe { camera.Remove() };
    }
    // System lifetime: releasing our object does not remove the device.
    // SAFETY: Live camera.
    let _ = unsafe { camera.Shutdown() };
    result
}

/// Removes the system camera of `name`. A camera that no longer exists (the
/// device was removed outside this library, or never started because Frame
/// Server could not load the source) counts as removed: reopening its tuple
/// then yields a fresh, never-started object whose `Remove` reports
/// `MF_E_INVALIDREQUEST`, which would otherwise leave a stale record that can
/// never be unregistered.
fn remove_camera(name: &str) -> Result<()> {
    let camera = virtual_camera(name)?;
    // SAFETY: Live camera reopened by its creation tuple.
    let removed = match unsafe { camera.Remove() } {
        Err(error) if error.code() == windows::Win32::Media::MediaFoundation::MF_E_INVALIDREQUEST => Ok(()),
        result => result.map_err(|error| hresult_error("IMFVirtualCamera::Remove", &error)),
    };
    // SAFETY: Live camera.
    let _ = unsafe { camera.Shutdown() };
    removed
}

pub(super) fn register(app_id: Option<AppId>, request: MediaFoundationRegistration) -> Result<CameraRecord> {
    if installed_source().is_none() {
        return Err(Error::BackendUnavailable(CameraType::ManagedMediaFoundation));
    }
    let pixel_formats = request.formats()?;
    let _scope = MfScope::enter()?;
    let id = super::imp::new_camera_id()?;
    let rendezvous_id = format!("{:032X}", u128::from_le_bytes(random_bytes()?));
    let owner = current_user_sid()?;
    let symbolic_link = create_camera(&request.name, id, &rendezvous_id, &owner, &request.default_format, &pixel_formats)?;
    let record = CameraRecord {
        id,
        hive: Hive::CurrentUser,
        backend: ManagedBackend::MediaFoundation,
        name: request.name,
        app_id,
        format: Some(request.default_format),
        pixel_formats,
        frame_rates: Vec::new(),
        source_path: None,
        rendezvous_id: Some(rendezvous_id),
        symbolic_link,
    };
    if let Err(error) = record.write() {
        let _ = remove_camera(&record.name);
        return Err(error);
    }
    Ok(record)
}

pub(super) fn unregister(record: &CameraRecord) -> Result<()> {
    let _scope = MfScope::enter()?;
    remove_camera(&record.name)
}

/// Rename = recreate under the new name with the same id, rendezvous, default
/// format and declared format set, then remove the old device (rolled back if
/// that fails).
pub(super) fn rename(record: &CameraRecord, name: &str) -> Result<CameraRecord> {
    let _scope = MfScope::enter()?;
    let (format, rendezvous_id) = match (record.format, record.rendezvous_id.as_deref()) {
        (Some(format), Some(rendezvous_id)) => (format, rendezvous_id),
        _ => {
            return Err(Error::RegistrationDegraded {
                detail: "Media Foundation record is incomplete",
            });
        }
    };
    if record.pixel_formats.is_empty() {
        return Err(Error::RegistrationDegraded {
            detail: "Media Foundation record declares no pixel format",
        });
    }
    let owner = current_user_sid()?;
    let symbolic_link = create_camera(name, record.id, rendezvous_id, &owner, &format, &record.pixel_formats)?;
    if let Err(error) = remove_camera(&record.name) {
        let _ = remove_camera(name);
        return Err(error);
    }
    let renamed = CameraRecord {
        name: name.to_owned(),
        symbolic_link,
        ..record.clone()
    };
    renamed.write()?;
    Ok(renamed)
}

pub(super) fn health(record: &CameraRecord) -> core::result::Result<(), &'static str> {
    if installed_source().is_none() {
        return Err("Media Foundation source is not installed");
    }
    if record.rendezvous_id.is_none() || record.format.is_none() {
        return Err("Media Foundation record is incomplete");
    }
    if record.pixel_formats.is_empty() {
        return Err("Media Foundation record declares no pixel format");
    }
    Ok(())
}

pub(super) fn open(record: &CameraRecord, format: ValidatedFormat) -> Result<VirtualCamera> {
    let rendezvous_id = record.rendezvous_id.as_deref().ok_or(Error::RegistrationDegraded {
        detail: "Media Foundation record is incomplete",
    })?;
    VirtualCamera::open_managed_mf(record.id, format, rendezvous_id)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use virtualcam_protocol::planes::fill_black;
    use virtualcam_protocol::{ColorSpace, FrameRate, PixelFormat};
    use windows::Win32::Media::MediaFoundation::{IMFActivate, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_SUBTYPE, MFCreateSourceReaderFromMediaSource};
    use windows::Win32::System::Com::IClassFactory;

    use super::*;
    use crate::WriteStatus;

    const FIRST_VIDEO_STREAM: u32 = 0xFFFF_FFFC;
    /// Every format a camera may declare, in declaration order.
    const FIVE: [PixelFormat; 5] = [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::P010];
    /// The same set in descriptor order: packed 4:2:2 before planar 4:2:0.
    const ORDERED: [PixelFormat; 5] = [PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010];

    fn read(reader: &IMFSourceReader) -> Vec<u8> {
        let mut sample: Option<IMFSample> = None;
        let mut flags = 0_u32;
        // SAFETY: Synchronous read into valid outputs.
        unsafe { reader.ReadSample(FIRST_VIDEO_STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) }.expect("ReadSample");
        let buffer = unsafe { sample.expect("sample").ConvertToContiguousBuffer() }.expect("buffer");
        let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
        // SAFETY: Lock/unlock pair on a valid buffer.
        unsafe {
            buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
            let bytes = core::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock().expect("unlock");
            bytes
        }
    }

    fn read_until(reader: &IMFSourceReader, want: impl Fn(&[u8]) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if want(&read(reader)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn write_until_attached(camera: &mut VirtualCamera, frame: &[u8]) -> Result<WriteStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = camera.write_host(frame)?;
            if status == WriteStatus::Written || Instant::now() > deadline {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    /// A deterministic frame of `pixel_format`; it never equals the black
    /// fill, so a delivered frame is distinguishable from an idle one.
    fn frame(pixel_format: PixelFormat, width: u32, height: u32) -> Vec<u8> {
        let size = pixel_format.layout(width, height).expect("layout").size();
        (0..size).map(|index| (index.wrapping_mul(37).wrapping_add(11) % 251) as u8).collect()
    }

    fn black(pixel_format: PixelFormat, width: u32, height: u32) -> Vec<u8> {
        let layout = pixel_format.layout(width, height).expect("layout");
        let mut bytes = vec![0_u8; layout.size()];
        fill_black(&mut bytes, &layout, pixel_format, ColorSpace::default_for(pixel_format, height).is_full_range(pixel_format)).expect("black");
        bytes
    }

    /// The media types the source offers, in enumeration order.
    fn offered(reader: &IMFSourceReader) -> Vec<PixelFormat> {
        (0..)
            .map_while(|index| {
                // SAFETY: Valid reader; a missing index ends the enumeration.
                unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index) }.ok()
            })
            .map(|media_type| {
                // SAFETY: Valid media type.
                let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.expect("subtype");
                PixelFormat::from_mf_subtype(subtype.to_u128()).expect("managed subtype")
            })
            .collect()
    }

    fn type_for(reader: &IMFSourceReader, pixel_format: PixelFormat) -> IMFMediaType {
        let subtype = GUID::from_u128(pixel_format.mf_subtype().expect("subtype"));
        (0..)
            .map_while(|index| {
                // SAFETY: Valid reader; a missing index ends the enumeration.
                unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index) }.ok()
            })
            // SAFETY: Valid media type.
            .find(|media_type| unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|guid| guid == subtype))
            .expect("declared type offered")
    }

    /// Selects `pixel_format` on a running reader, as a consumer switching
    /// modes does: no source is recreated and no camera re-registered.
    fn select(reader: &IMFSourceReader, pixel_format: PixelFormat) {
        let media_type = type_for(reader, pixel_format);
        // SAFETY: An offered media type on a live reader.
        unsafe { reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &media_type) }.expect("select");
    }

    /// An in-process source for a fresh camera id and rendezvous, declaring
    /// `formats` with `default` as the default mode, with a reader whose
    /// current type is `selected`.
    fn in_process_camera(default: &VideoFormat, formats: &[PixelFormat], selected: PixelFormat) -> (ManagedCameraId, String, IMFMediaSource, IMFSourceReader) {
        let mut factory = core::ptr::null_mut();
        let clsid = GUID::from_u128(SOURCE_CLSID.to_u128());
        // SAFETY: Valid pointers for the export of the linked source crate.
        assert_eq!(unsafe { virtualcam_mf::DllGetClassObject(&clsid, &IClassFactory::IID, &mut factory) }, windows::Win32::Foundation::S_OK);
        // SAFETY: One owned factory reference.
        let factory = unsafe { IClassFactory::from_raw(factory) };
        // SAFETY: Valid factory.
        let activate: IMFActivate = unsafe { factory.CreateInstance(None) }.expect("activate");
        let id = super::super::imp::new_camera_id().expect("id");
        let rendezvous = format!("{:032X}", u128::from_le_bytes(random_bytes().expect("random")));
        configure_attributes(&activate.cast().expect("attributes"), id, &rendezvous, &current_user_sid().expect("sid"), default, formats).expect("configure");
        // SAFETY: Valid activate, source and reader for the rest of the call.
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject().expect("source");
            let reader = MFCreateSourceReaderFromMediaSource(&source, None).expect("reader");
            select(&reader, selected);
            (id, rendezvous, source, reader)
        }
    }

    /// Writes until the source's request for `wanted` reaches the producer.
    fn write_until_requested(camera: &mut VirtualCamera, frame: &[u8], wanted: Option<VideoFormat>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let _ = camera.write_host(frame);
            if camera.requested_format() == wanted {
                return true;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        false
    }

    /// The camera's declared mode for `pixel_format`: every mode shares the
    /// default format's size, rate and colorimetry.
    fn mode(default: &VideoFormat, pixel_format: PixelFormat) -> VideoFormat {
        VideoFormat { pixel_format, ..*default }
    }

    /// The declared set is enumerated in bridge-safe order, a producer may
    /// open any declared mode, and a frame in a mode consumers did not select
    /// is delivered as black — never converted.
    #[test]
    fn producer_follows_the_mode_consumers_selected() {
        let _scope = MfScope::enter().expect("MF");
        let (width, height) = (64, 48);
        let default = VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(30));
        let yuy2 = mode(&default, PixelFormat::Yuy2);
        let validated = |format: VideoFormat| format.validate().expect("valid");

        // Consumers selected YUY2; a producer writing the default NV12 keeps
        // running but is shown as black, and is asked for YUY2.
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::Yuy2);
        assert_eq!(offered(&reader), ORDERED, "every declared mode is offered, packed 4:2:2 first");
        let nv12_frame = frame(PixelFormat::Nv12, width, height);
        let mut camera = VirtualCamera::open_managed_mf(id, validated(default), &rendezvous).expect("open default mode");
        assert_eq!(write_until_attached(&mut camera, &nv12_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| *bytes == black(PixelFormat::Yuy2, width, height)), "an NV12 frame is never converted into the selected YUY2");
        assert!(write_until_requested(&mut camera, &nv12_frame, Some(yuy2)), "producer asked for YUY2");

        // Reopened in YUY2: the source copies the producer's frame unchanged.
        camera.close().expect("close");
        let yuy2_frame = frame(PixelFormat::Yuy2, width, height);
        let mut camera = VirtualCamera::open_managed_mf(id, validated(yuy2), &rendezvous).expect("open YUY2");
        assert_eq!(write_until_attached(&mut camera, &yuy2_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| *bytes == yuy2_frame), "the producer's YUY2 frame is delivered unchanged");
        assert!(write_until_requested(&mut camera, &yuy2_frame, None), "nothing more to follow");
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };

        // A producer may open a mode that is not the default one even when
        // consumers already selected it: no request is pending.
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::P010);
        let p010_frame = frame(PixelFormat::P010, width, height);
        let mut camera = VirtualCamera::open_managed_mf(id, validated(mode(&default, PixelFormat::P010)), &rendezvous).expect("open P010");
        assert_eq!(write_until_attached(&mut camera, &p010_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| *bytes == p010_frame), "a non-default declared mode is delivered");
        assert_eq!(camera.requested_format(), None, "the selected mode is already being written");
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };

        // Modes the camera does not declare are still refused.
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::Nv12);
        let bgra = mode(&default, PixelFormat::Bgra32);
        let refused = VirtualCamera::open_managed_mf(id, validated(bgra), &rendezvous).and_then(|mut camera| write_until_attached(&mut camera, &vec![0; validated(bgra).frame_size()]));
        assert_eq!(refused, Err(Error::FormatMismatch));
        drop(reader);
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
    }

    /// Consumers switch between declared modes repeatedly on one source; the
    /// producer follows each selection by reopening, and every mode arrives
    /// byte for byte.
    #[test]
    fn consumers_switch_between_declared_modes() {
        let _scope = MfScope::enter().expect("MF");
        let (width, height) = (64, 48);
        let default = VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(30));
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::Nv12);

        let mut camera = VirtualCamera::open_managed_mf(id, default.validate().expect("valid"), &rendezvous).expect("open default");
        let mut producing = PixelFormat::Nv12;
        let mut current = frame(producing, width, height);
        assert_eq!(write_until_attached(&mut camera, &current), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| *bytes == current), "the default mode is delivered");

        // A full round trip, ending back at the mode the producer started in.
        for selected in [PixelFormat::Yuy2, PixelFormat::I420, PixelFormat::Uyvy, PixelFormat::P010, PixelFormat::Nv12] {
            select(&reader, selected);
            let wanted = mode(&default, selected);
            // The producer keeps writing the old mode: the consumer sees black
            // and the source asks for the new one.
            if selected != producing {
                assert!(read_until(&reader, |bytes| *bytes == black(selected, width, height)), "{selected:?}: no frame in another mode is delivered");
                assert!(write_until_requested(&mut camera, &current, Some(wanted)), "{selected:?}: producer asked to switch");
                camera.close().expect("close");
                camera = VirtualCamera::open_managed_mf(id, wanted.validate().expect("valid"), &rendezvous).expect("reopen in the selected mode");
                producing = selected;
                current = frame(selected, width, height);
            }
            assert_eq!(write_until_attached(&mut camera, &current), Ok(WriteStatus::Written), "{selected:?}");
            assert!(read_until(&reader, |bytes| *bytes == current), "{selected:?}: delivered unchanged");
            assert_eq!(camera.requested_format(), None, "{selected:?}: nothing pending");
        }
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
    }

    /// A frame written in another declared mode is paced as black, not
    /// reinterpreted and not converted.
    #[test]
    fn mismatched_frames_are_black_and_paced() {
        let _scope = MfScope::enter().expect("MF");
        let (width, height) = (64, 48);
        let default = VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(30));
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::Nv12);
        let producing = mode(&default, PixelFormat::Yuy2);
        let mut camera = VirtualCamera::open_managed_mf(id, producing.validate().expect("format"), &rendezvous).expect("open");
        assert_eq!(write_until_attached(&mut camera, &frame(PixelFormat::Yuy2, width, height)), Ok(WriteStatus::Written));
        let expected = black(PixelFormat::Nv12, width, height);
        let started = Instant::now();
        let mut samples = 0;
        while started.elapsed() < Duration::from_millis(250) {
            assert_eq!(read(&reader), expected, "a YUY2 frame is never delivered as NV12");
            samples += 1;
        }
        drop((camera, reader));
        // SAFETY: Valid source; the reader may already have shut it down.
        let _ = unsafe { source.Shutdown() };
        assert!(samples <= 12, "{samples} black samples from one producer frame in {:?}", started.elapsed());
    }

    /// A consumer that reads as fast as it can gets the producer's frames
    /// at the producer's rate, not one sample per request.
    #[test]
    fn delivery_follows_the_producer_rate() {
        let _scope = MfScope::enter().expect("MF");
        let native = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        let (id, rendezvous, source, reader) = in_process_camera(&native, &[PixelFormat::Nv12], PixelFormat::Nv12);
        let frame = vec![100_u8; 64 * 48 * 3 / 2];
        let mut camera = VirtualCamera::open_managed_mf(id, native.validate().expect("valid"), &rendezvous).expect("open");
        assert_eq!(write_until_attached(&mut camera, &frame), Ok(WriteStatus::Written));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let producer = {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = camera.write_host(&frame);
                    std::thread::sleep(Duration::from_millis(33));
                }
            })
        };
        let started = Instant::now();
        let mut samples = 0;
        while started.elapsed() < Duration::from_secs(1) {
            let _ = read(&reader);
            samples += 1;
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        producer.join().expect("producer");
        assert!((15..=45).contains(&samples), "{samples} samples in one second from a 30 FPS producer");
        drop(reader);
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
    }

    /// Unregistering a record whose system camera is already gone succeeds,
    /// so the record can be cleaned up instead of failing forever.
    #[test]
    #[ignore = "creates (never starts) a Windows virtual camera object"]
    fn removing_a_missing_system_camera_counts_as_removed() {
        let _scope = MfScope::enter().expect("MF");
        let name = format!("VCRs Missing {}", std::process::id());
        assert_eq!(remove_camera(&name), Ok(()));
    }

    #[test]
    fn producer_authenticates_and_feeds_an_in_process_frame_server_source() {
        let _scope = MfScope::enter().expect("MF");
        let mut factory = core::ptr::null_mut();
        let clsid = GUID::from_u128(SOURCE_CLSID.to_u128());
        // SAFETY: Valid pointers for the export of the linked source crate.
        assert_eq!(unsafe { virtualcam_mf::DllGetClassObject(&clsid, &IClassFactory::IID, &mut factory) }, windows::Win32::Foundation::S_OK);
        // SAFETY: One owned factory reference.
        let factory = unsafe { IClassFactory::from_raw(factory) };
        // SAFETY: Valid factory.
        let activate: IMFActivate = unsafe { factory.CreateInstance(None) }.expect("activate");

        let id = super::super::imp::new_camera_id().expect("id");
        let rendezvous = format!("{:032X}", u128::from_le_bytes(random_bytes().expect("random")));
        let format = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        configure_attributes(&activate.cast().expect("attributes"), id, &rendezvous, &current_user_sid().expect("sid"), &format, &[PixelFormat::Nv12]).expect("configure");
        // SAFETY: Valid activate.
        let source: IMFMediaSource = unsafe { activate.ActivateObject() }.expect("source");
        // SAFETY: Valid source.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }.expect("reader");
        let (width, height) = (64, 48);
        let is_black = |bytes: &[u8]| *bytes == black(PixelFormat::Nv12, width, height);
        assert!(is_black(&read(&reader)), "no producer: black");

        // A different size, and a pixel format the camera does not declare,
        // are both rejected by the source.
        let other = VideoFormat::new(PixelFormat::Nv12, 32, 48, FrameRate::fps(30)).validate().expect("valid");
        // (The rejection surfaces at `open` when the source is already up,
        // otherwise at the first write that reaches it.)
        let attempt = |id, format: ValidatedFormat| VirtualCamera::open_managed_mf(id, format, &rendezvous).and_then(|mut camera| write_until_attached(&mut camera, &vec![0; format.frame_size()]));
        assert_eq!(attempt(id, other), Err(Error::FormatMismatch));
        assert_eq!(attempt(id, mode(&format, PixelFormat::Yuy2).validate().expect("valid")), Err(Error::FormatMismatch), "an undeclared mode is refused");
        let stranger = super::super::imp::new_camera_id().expect("id");
        assert!(matches!(attempt(stranger, format.validate().expect("valid")), Err(Error::Protocol { .. })));

        // The owner attaches and its frames reach the source reader.
        let produced = frame(PixelFormat::Nv12, width, height);
        let mut camera = VirtualCamera::open_managed_mf(id, format.validate().expect("valid"), &rendezvous).expect("open");
        assert_eq!(write_until_attached(&mut camera, &produced), Ok(WriteStatus::Written));
        assert_eq!(camera.is_ready(), Ok(true));
        assert!(read_until(&reader, |bytes| *bytes == produced), "producer frame delivered");

        // Closing the producer closes the pipe: the source returns to black.
        camera.close().expect("close");
        assert!(read_until(&reader, is_black), "black after producer close");

        // A new producer session can attach again.
        let mut camera = VirtualCamera::open_managed_mf(id, format.validate().expect("valid"), &rendezvous).expect("reopen");
        assert_eq!(write_until_attached(&mut camera, &produced), Ok(WriteStatus::Written));
        // Releasing the reader shuts the source down (reader default); an
        // explicit Shutdown afterwards reports MF_E_SHUTDOWN.
        drop(reader);
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
        // The source closed the pipe; the producer notices and goes inactive.
        let deadline = Instant::now() + Duration::from_secs(2);
        while camera.write_host(&produced) == Ok(WriteStatus::Written) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(camera.write_host(&produced), Ok(WriteStatus::ConsumerInactive));
    }

    /// A device-backed frame reaches consumers unchanged, in whatever mode the
    /// session writes: `write_cuda` copies D2H straight into the ring slot and
    /// never stages or converts.
    #[cfg(feature = "cuda")]
    #[test]
    #[ignore = "requires a CUDA-capable device and Runtime"]
    fn write_cuda_delivers_every_selected_mode_without_conversion() {
        use core::ffi::c_void;

        // CUDA Runtime entry points for the device side of the copy.
        type Malloc = unsafe extern "system" fn(*mut *mut c_void, usize) -> i32;
        type Free = unsafe extern "system" fn(*mut c_void) -> i32;
        type Memcpy = unsafe extern "system" fn(*mut c_void, *const c_void, usize, i32) -> i32;
        const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
        // SAFETY: Each type is the documented CUDA Runtime signature of its export.
        let (cuda_malloc, cuda_free, cuda_memcpy) = unsafe { (crate::cuda::test_api::<Malloc>(c"cudaMalloc"), crate::cuda::test_api::<Free>(c"cudaFree"), crate::cuda::test_api::<Memcpy>(c"cudaMemcpy")) };
        // A device buffer holding one frame, released on drop.
        struct Device {
            pointer: *mut c_void,
            len: usize,
            free: Free,
        }
        impl Device {
            fn upload(host: &[u8], malloc: Malloc, memcpy: Memcpy, free: Free) -> Self {
                let mut pointer = core::ptr::null_mut();
                // SAFETY: Writable pointer storage receives an allocation of exactly `host.len()` bytes.
                assert_eq!(unsafe { malloc(&mut pointer, host.len()) }, 0, "cudaMalloc");
                assert!(!pointer.is_null());
                // SAFETY: Both buffers hold `host.len()` bytes and kind 1 is H2D.
                assert_eq!(unsafe { memcpy(pointer, host.as_ptr().cast(), host.len(), CUDA_MEMCPY_HOST_TO_DEVICE) }, 0, "cudaMemcpy");
                Self { pointer, len: host.len(), free }
            }
        }
        impl Drop for Device {
            fn drop(&mut self) {
                // SAFETY: The pointer came from a successful cudaMalloc and is freed once.
                assert_eq!(unsafe { (self.free)(self.pointer) }, 0, "cudaFree");
            }
        }

        let _scope = MfScope::enter().expect("MF");
        let (width, height) = (64, 48);
        let default = VideoFormat::new(PixelFormat::Nv12, width, height, FrameRate::fps(30));
        let (id, rendezvous, source, reader) = in_process_camera(&default, &FIVE, PixelFormat::Nv12);
        let mut camera = VirtualCamera::open_managed_mf(id, default.validate().expect("valid"), &rendezvous).expect("open default");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut producing = PixelFormat::Nv12;
        loop {
            let host = frame(producing, width, height);
            let device = Device::upload(&host, cuda_malloc, cuda_memcpy, cuda_free);
            // SAFETY: The device allocation spans exactly `device.len` readable
            // bytes in the context current on this thread, and no stream is passed.
            let status = unsafe { camera.write_cuda(device.pointer, device.len, None) }.expect("write_cuda");
            if status == WriteStatus::Written {
                assert!(read_until(&reader, |bytes| *bytes == host), "{producing:?}: the device frame arrives unchanged");
                break;
            }
            assert!(Instant::now() < deadline, "producer never attached: {status:?}");
            std::thread::sleep(Duration::from_millis(30));
        }

        // The same D2H path in another selected mode: no conversion anywhere.
        for selected in [PixelFormat::Yuy2, PixelFormat::P010] {
            select(&reader, selected);
            // The source reader applies a new type on its next read, which is
            // what restarts the source and signals the selection.
            let _ = read(&reader);
            let wanted = mode(&default, selected);
            assert!(write_until_requested(&mut camera, &frame(producing, width, height), Some(wanted)), "{selected:?}: producer asked to switch");
            camera.close().expect("close");
            camera = VirtualCamera::open_managed_mf(id, wanted.validate().expect("valid"), &rendezvous).expect("reopen");
            producing = selected;
            let host = frame(producing, width, height);
            let device = Device::upload(&host, cuda_malloc, cuda_memcpy, cuda_free);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // SAFETY: As above; re-uploaded for every attempt.
                match unsafe { camera.write_cuda(device.pointer, device.len, None) }.expect("write_cuda") {
                    WriteStatus::Written => break,
                    status => {
                        assert!(Instant::now() < deadline, "{selected:?}: producer never attached: {status:?}");
                        std::thread::sleep(Duration::from_millis(30));
                    }
                }
            }
            assert!(read_until(&reader, |bytes| *bytes == host), "{selected:?}: the device frame arrives unchanged");
        }
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
    }
}
