//! Media Foundation camera management.
//!
//! Every camera is created with `MFCreateVirtualCamera(SoftwareCameraSource,
//! System lifetime, CurrentUser access, name, fixed source CLSID)` and
//! configured through vendor attributes (camera id, rendezvous id, owner SID
//! assertion, protocol version, native mode). The same tuple reopens the
//! same system camera, which is how unregister and rename find it again.
//! The manager never needs elevation for cameras; installing the shared
//! source DLL machine-wide (once) does.

use std::path::{Path, PathBuf};

use virtualcam_protocol::mf::{ATTR_CAMERA_ID, ATTR_EXPECTED_OWNER_SID, ATTR_NATIVE_FORMAT, ATTR_PROTOCOL_VERSION, ATTR_RENDEZVOUS_ID, PROTOCOL_VERSION, SOURCE_CLSID};
use virtualcam_protocol::{ValidatedFormat, VideoFormat};
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
pub(crate) fn configure_attributes(attributes: &IMFAttributes, id: ManagedCameraId, rendezvous_id: &str, owner_sid: &str, format: &VideoFormat) -> Result<()> {
    let key = |id: ManagedCameraId| GUID::from_u128(id.to_u128());
    let wire = format.to_wire()?.to_bytes();
    // SAFETY: Plain attribute writes on a live store.
    let result: windows_core::Result<()> = unsafe {
        (|| {
            attributes.SetString(&key(ATTR_CAMERA_ID), &HSTRING::from(id.to_string()))?;
            attributes.SetString(&key(ATTR_RENDEZVOUS_ID), &HSTRING::from(rendezvous_id))?;
            attributes.SetString(&key(ATTR_EXPECTED_OWNER_SID), &HSTRING::from(owner_sid))?;
            attributes.SetUINT32(&key(ATTR_PROTOCOL_VERSION), PROTOCOL_VERSION)?;
            attributes.SetBlob(&key(ATTR_NATIVE_FORMAT), &wire)
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
fn create_camera(name: &str, id: ManagedCameraId, rendezvous_id: &str, owner_sid: &str, format: &VideoFormat) -> Result<Option<String>> {
    let camera = virtual_camera(name)?;
    let result = (|| {
        configure_attributes(
            &camera.cast().map_err(|error| hresult_error("IMFVirtualCamera::QueryInterface(IMFAttributes)", &error))?,
            id,
            rendezvous_id,
            owner_sid,
            format,
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
    let _scope = MfScope::enter()?;
    let id = super::imp::new_camera_id()?;
    let rendezvous_id = format!("{:032X}", u128::from_le_bytes(random_bytes()?));
    let owner = current_user_sid()?;
    let symbolic_link = create_camera(&request.name, id, &rendezvous_id, &owner, &request.native_format)?;
    let record = CameraRecord {
        id,
        hive: Hive::CurrentUser,
        backend: ManagedBackend::MediaFoundation,
        name: request.name,
        app_id,
        format: Some(request.native_format),
        pixel_formats: Vec::new(),
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

/// Rename = recreate under the new name with the same id, rendezvous, and
/// native mode, then remove the old device (rolled back if that fails).
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
    let owner = current_user_sid()?;
    let symbolic_link = create_camera(name, record.id, rendezvous_id, &owner, &format)?;
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

    use virtualcam_protocol::{FrameRate, PixelFormat};
    use windows::Win32::Media::MediaFoundation::{IMFActivate, IMFMediaSource, IMFSample, IMFSourceReader, MFCreateSourceReaderFromMediaSource};
    use windows::Win32::System::Com::IClassFactory;

    use super::*;
    use crate::WriteStatus;

    const FIRST_VIDEO_STREAM: u32 = 0xFFFF_FFFC;

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

    /// An in-process source for a fresh camera id and rendezvous, with a
    /// reader whose current type is `selected`.
    fn in_process_camera(native: VideoFormat, selected: PixelFormat) -> (ManagedCameraId, String, IMFMediaSource, IMFSourceReader) {
        use windows::Win32::Media::MediaFoundation::MF_MT_SUBTYPE;
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
        configure_attributes(&activate.cast().expect("attributes"), id, &rendezvous, &current_user_sid().expect("sid"), &native).expect("configure");
        // SAFETY: Valid activate, source and reader for the rest of the call.
        unsafe {
            let source: IMFMediaSource = activate.ActivateObject().expect("source");
            let reader = MFCreateSourceReaderFromMediaSource(&source, None).expect("reader");
            let subtype = GUID::from_u128(selected.mf_subtype().expect("subtype"));
            let media_type = (0..)
                .map_while(|index| reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index).ok())
                .find(|media_type| media_type.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|guid| guid == subtype))
                .expect("selected type offered");
            reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &media_type).expect("select");
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

    /// The source asks the producer for the consumers' selected mode; the
    /// producer may write any offered mode. Meanwhile the source converts
    /// when it can (NV12 -> YUY2) and shows black when it cannot.
    #[test]
    fn producer_follows_the_mode_consumers_selected() {
        let _scope = MfScope::enter().expect("MF");
        let native = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        let yuy2 = VideoFormat { pixel_format: PixelFormat::Yuy2, ..native };
        let y = 64 * 48;
        let nv12_frame: Vec<u8> = (0..y * 3 / 2).map(|i| (i * 13 + i / 64) as u8).collect();
        let yuy2_frame: Vec<u8> = (0..y * 2).map(|i| (i * 7 + 3) as u8).collect();
        let mut converted = vec![0_u8; y * 2];
        let (nv12_layout, yuy2_layout) = (native.pixel_format.layout(64, 48).expect("layout"), yuy2.pixel_format.layout(64, 48).expect("layout"));
        // SAFETY: `nv12_frame` spans the tight NV12 layout.
        unsafe { virtualcam_protocol::convert::convert_frame(nv12_frame.as_ptr(), PixelFormat::Nv12, &nv12_layout, &mut converted, PixelFormat::Yuy2, &yuy2_layout) }.expect("convert");
        let validated = |format: VideoFormat| format.validate().expect("valid");

        // Consumers selected YUY2; a native producer is converted by the
        // source and asked for YUY2.
        let (id, rendezvous, source, reader) = in_process_camera(native, PixelFormat::Yuy2);
        let mut camera = VirtualCamera::open_managed_mf(id, validated(native), &rendezvous).expect("open native");
        assert_eq!(write_until_attached(&mut camera, &nv12_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| bytes == converted.as_slice()), "source converts NV12 to the selected YUY2");
        assert!(write_until_requested(&mut camera, &nv12_frame, Some(yuy2)), "producer asked for YUY2");
        // Reopened in YUY2: the source copies the producer's frame as is.
        camera.close().expect("close");
        let mut camera = VirtualCamera::open_managed_mf(id, validated(yuy2), &rendezvous).expect("open YUY2");
        assert_eq!(write_until_attached(&mut camera, &yuy2_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| bytes == yuy2_frame.as_slice()), "producer's YUY2 delivered unchanged");
        assert!(write_until_requested(&mut camera, &yuy2_frame, None), "nothing more to follow");
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };

        // Consumers selected NV12; a YUY2 producer cannot be converted back:
        // black until it follows the request.
        let (id, rendezvous, source, reader) = in_process_camera(native, PixelFormat::Nv12);
        let mut camera = VirtualCamera::open_managed_mf(id, validated(yuy2), &rendezvous).expect("open YUY2");
        assert_eq!(write_until_attached(&mut camera, &yuy2_frame), Ok(WriteStatus::Written));
        let is_black = |bytes: &[u8]| bytes.len() == y * 3 / 2 && bytes[..y].iter().all(|v| *v == 16) && bytes[y..].iter().all(|v| *v == 128);
        assert!(is_black(&read(&reader)), "YUY2 cannot become NV12: black");
        assert!(write_until_requested(&mut camera, &yuy2_frame, Some(native)), "producer asked for NV12");
        camera.close().expect("close");
        let mut camera = VirtualCamera::open_managed_mf(id, validated(native), &rendezvous).expect("open native");
        assert_eq!(write_until_attached(&mut camera, &nv12_frame), Ok(WriteStatus::Written));
        assert!(read_until(&reader, |bytes| bytes == nv12_frame.as_slice()), "native frame delivered");
        drop((camera, reader));
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };

        // Modes the source does not offer are still refused.
        let (id, rendezvous, source, reader) = in_process_camera(native, PixelFormat::Nv12);
        let bgra = VideoFormat {
            pixel_format: PixelFormat::Bgra32,
            ..native
        };
        let refused = VirtualCamera::open_managed_mf(id, validated(bgra), &rendezvous).and_then(|mut camera| write_until_attached(&mut camera, &vec![0; validated(bgra).frame_size()]));
        assert_eq!(refused, Err(Error::FormatMismatch));
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

    /// A consumer that reads as fast as it can gets the producer's frames
    /// at the producer's rate, not one sample per request.
    #[test]
    fn delivery_follows_the_producer_rate() {
        let _scope = MfScope::enter().expect("MF");
        let native = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        let (id, rendezvous, source, reader) = in_process_camera(native, PixelFormat::Nv12);
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
        configure_attributes(&activate.cast().expect("attributes"), id, &rendezvous, &current_user_sid().expect("sid"), &format).expect("configure");
        // SAFETY: Valid activate.
        let source: IMFMediaSource = unsafe { activate.ActivateObject() }.expect("source");
        // SAFETY: Valid source.
        let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, None) }.expect("reader");
        let y = 64 * 48;
        let is_black = |bytes: &[u8]| bytes.len() == y * 3 / 2 && bytes[..y].iter().all(|v| *v == 16) && bytes[y..].iter().all(|v| *v == 128);
        assert!(is_black(&read(&reader)), "no producer: black");

        // Wrong native mode and wrong camera are rejected by the source.
        let other = VideoFormat::new(PixelFormat::Nv12, 32, 48, FrameRate::fps(30)).validate().expect("valid");
        // (The rejection surfaces at `open` when the source is already up,
        // otherwise at the first write that reaches it.)
        let attempt = |id, format: ValidatedFormat| VirtualCamera::open_managed_mf(id, format, &rendezvous).and_then(|mut camera| write_until_attached(&mut camera, &vec![0; format.frame_size()]));
        assert_eq!(attempt(id, other), Err(Error::FormatMismatch));
        let stranger = super::super::imp::new_camera_id().expect("id");
        assert!(matches!(attempt(stranger, format.validate().expect("valid")), Err(Error::Protocol { .. })));

        // The owner attaches and its frames reach the source reader.
        let frame: Vec<u8> = (0..y * 3 / 2).map(|i| (i * 13 + i / 64) as u8).collect();
        let mut camera = VirtualCamera::open_managed_mf(id, format.validate().expect("valid"), &rendezvous).expect("open");
        assert_eq!(write_until_attached(&mut camera, &frame), Ok(WriteStatus::Written));
        assert_eq!(camera.is_ready(), Ok(true));
        assert!(read_until(&reader, |bytes| bytes == frame.as_slice()), "producer frame delivered");

        // Closing the producer closes the pipe: the source returns to black.
        camera.close().expect("close");
        assert!(read_until(&reader, is_black), "black after producer close");

        // A new producer session can attach again.
        let mut camera = VirtualCamera::open_managed_mf(id, format.validate().expect("valid"), &rendezvous).expect("reopen");
        assert_eq!(write_until_attached(&mut camera, &frame), Ok(WriteStatus::Written));
        // Releasing the reader shuts the source down (reader default); an
        // explicit Shutdown afterwards reports MF_E_SHUTDOWN.
        drop(reader);
        // SAFETY: Valid source.
        let _ = unsafe { source.Shutdown() };
        // The source closed the pipe; the producer notices and goes inactive.
        let deadline = Instant::now() + Duration::from_secs(2);
        while camera.write_host(&frame) == Ok(WriteStatus::Written) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(camera.write_host(&frame), Ok(WriteStatus::ConsumerInactive));
    }
}
