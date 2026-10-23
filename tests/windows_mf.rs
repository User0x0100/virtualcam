//! End-to-end test of a real Frame Server virtual camera.
//!
//! Requires the Media Foundation source to be installed machine-wide once
//! (elevated), from a location Local Service can read:
//!
//! ```text
//! cargo build -p virtualcam-mf --release
//! copy target\release\virtualcam_mf.dll "C:\Program Files\VirtualCamRs\"
//! # elevated: ManagedCameraManager::install_media_foundation_source(...)
//! cargo test --test windows_mf -- --ignored
//! ```

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use virtualcam::{AppId, FrameRate, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, RegistrationHealth, VideoFormat, WriteStatus};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFSample, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_VERSION, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows_core::PWSTR;

fn video_devices() -> Vec<(String, IMFActivate)> {
    let mut attributes: Option<IMFAttributes> = None;
    let mut devices: *mut Option<IMFActivate> = core::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: Standard device enumeration; the array is freed below.
    unsafe {
        MFCreateAttributes(&mut attributes, 1).expect("attributes");
        let attributes = attributes.expect("attributes");
        attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).expect("vidcap");
        MFEnumDeviceSources(&attributes, &mut devices, &mut count).expect("enumerate");
        let list = (0..count as usize)
            .filter_map(|index| {
                let device = (*devices.add(index)).take()?;
                let mut name = PWSTR::null();
                let mut length = 0;
                device.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut length).ok()?;
                let text = name.to_string().ok();
                CoTaskMemFree(Some(name.0.cast()));
                Some((text?, device))
            })
            .collect();
        CoTaskMemFree(Some(devices.cast()));
        list
    }
}

#[test]
#[ignore = "requires the Media Foundation source installed machine-wide (elevated, once)"]
fn frame_server_camera_delivers_producer_frames() {
    // SAFETY: Test thread setup.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    assert!(ManagedCameraManager::media_foundation_source().is_some(), "install the Media Foundation source first");
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-test").expect("app id"));
    let name = format!("VCRs MF Test {}", std::process::id());
    let format = VideoFormat::new(PixelFormat::Nv12, 640, 360, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new(&name, format)).expect("register MF camera");
    assert_eq!(info.health, RegistrationHealth::Healthy);
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager.clone(), info.id);

    // Producer runs continuously; it attaches once the camera is opened.
    let frame: Vec<u8> = (0..640 * 360 * 3 / 2).map(|i| (i % 251) as u8).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let (manager, frame, stop) = (manager.clone(), frame.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut camera = manager.open(info.id, format).expect("open producer");
            let mut written = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                if camera.write_host(&frame).expect("write") == WriteStatus::Written {
                    written += 1;
                }
                std::thread::sleep(Duration::from_millis(33));
            }
            written
        })
    };

    let (_, device) = video_devices().into_iter().find(|(device, _)| *device == name).expect("Frame Server lists the camera");
    // SAFETY: Activating the enumerated device and reading synchronously.
    let matched = unsafe {
        let source = device.ActivateObject::<windows::Win32::Media::MediaFoundation::IMFMediaSource>().expect("activate");
        let reader = MFCreateSourceReaderFromMediaSource(&source, None).expect("reader");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut matched = false;
        while !matched && Instant::now() < deadline {
            let mut sample: Option<IMFSample> = None;
            let mut flags = 0;
            reader.ReadSample(0xFFFF_FFFC, 0, None, Some(&mut flags), None, Some(&mut sample)).expect("read");
            if let Some(sample) = sample {
                let buffer = sample.ConvertToContiguousBuffer().expect("buffer");
                let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
                buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
                matched = core::slice::from_raw_parts(data, length as usize) == frame.as_slice();
                buffer.Unlock().expect("unlock");
            }
        }
        let _ = source.Shutdown();
        matched
    };
    stop.store(true, Ordering::Relaxed);
    let written = producer.join().expect("producer");
    assert!(matched, "producer frame never arrived through Frame Server ({written} frames written)");

    manager.unregister(info.id).expect("unregister");
    assert!(video_devices().iter().all(|(device, _)| *device != name), "camera removed");
}
