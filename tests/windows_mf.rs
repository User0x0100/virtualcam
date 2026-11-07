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
    IMFActivate, IMFAttributes, IMFSample, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_SUBTYPE, MF_VERSION, MFCreateAttributes,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows_core::{GUID, PWSTR};

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

/// Frame Server appends a localized suffix such as " (Windows Virtual
/// Camera)" to the registered friendly name.
fn is_camera(device: &str, name: &str) -> bool {
    device.strip_prefix(name).is_some_and(|rest| rest.is_empty() || rest.starts_with(" ("))
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

    let (_, device) = video_devices().into_iter().find(|(device, _)| is_camera(device, &name)).expect("Frame Server lists the camera");
    // The YUY2 the source converts itself, for the same producer frame.
    let mut converted = vec![0_u8; 640 * 360 * 2];
    let (nv12_layout, yuy2_layout) = (PixelFormat::Nv12.layout(640, 360).expect("layout"), PixelFormat::Yuy2.layout(640, 360).expect("layout"));
    // SAFETY: `frame` spans the tight NV12 layout.
    unsafe { virtualcam_protocol::convert::convert_frame(frame.as_ptr(), PixelFormat::Nv12, &nv12_layout, &mut converted, PixelFormat::Yuy2, &yuy2_layout) }.expect("convert");
    let subtype = |format: PixelFormat| GUID::from_u128(format.mf_subtype().expect("subtype"));
    // SAFETY: Activating the enumerated device and reading synchronously.
    let (default_type, default_matched, native_matched) = unsafe {
        let source = device.ActivateObject::<windows::Win32::Media::MediaFoundation::IMFMediaSource>().expect("activate");
        let reader = MFCreateSourceReaderFromMediaSource(&source, None).expect("reader");
        let read_until = |expected: &[u8]| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let mut sample: Option<IMFSample> = None;
                let mut flags = 0;
                reader.ReadSample(0xFFFF_FFFC, 0, None, Some(&mut flags), None, Some(&mut sample)).expect("read");
                if let Some(sample) = sample {
                    let buffer = sample.ConvertToContiguousBuffer().expect("buffer");
                    let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
                    buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
                    let matched = core::slice::from_raw_parts(data, length as usize) == expected;
                    buffer.Unlock().expect("unlock");
                    if matched {
                        return true;
                    }
                }
            }
            false
        };
        // Frame Server makes the first listed type the default: the source's
        // own YUY2, listed before the native mode for the DirectShow bridge.
        let default_type = reader.GetCurrentMediaType(0).expect("current").GetGUID(&MF_MT_SUBTYPE).expect("subtype");
        let default_matched = read_until(&converted);
        // The native mode, selected explicitly, arrives byte for byte.
        let native = (0..)
            .map_while(|index| reader.GetNativeMediaType(0, index).ok())
            .find(|media_type| media_type.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|guid| guid == subtype(PixelFormat::Nv12)))
            .expect("native NV12 listed");
        reader.SetCurrentMediaType(0, None, &native).expect("select NV12");
        let native_matched = read_until(&frame);
        let _ = source.Shutdown();
        (default_type, default_matched, native_matched)
    };
    stop.store(true, Ordering::Relaxed);
    let written = producer.join().expect("producer");
    assert_eq!(default_type, subtype(PixelFormat::Yuy2), "default type is the first listed one");
    assert!(default_matched, "converted YUY2 never arrived through Frame Server ({written} frames written)");
    assert!(native_matched, "native NV12 frame never arrived through Frame Server ({written} frames written)");

    manager.unregister(info.id).expect("unregister");
    assert!(video_devices().iter().all(|(device, _)| !is_camera(device, &name)), "camera removed");
}

/// Captures the last of `frames` frames through FFmpeg's DirectShow input,
/// asking the device for `pix_fmt` (served by Frame Server's DirectShow
/// bridge for Media Foundation cameras).
fn directshow_capture(device: &str, pix_fmt: &str, frames: u32, frame_size: usize) -> Vec<u8> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-rtbufsize", "300M", "-f", "dshow", "-pixel_format", pix_fmt, "-i"])
        .arg(format!("video={device}"))
        .args(["-frames:v", &frames.to_string(), "-f", "rawvideo", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(output.status.success(), "ffmpeg {pix_fmt}: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.len() >= frame_size && output.stdout.len().is_multiple_of(frame_size), "{pix_fmt}: {} bytes", output.stdout.len());
    output.stdout[output.stdout.len() - frame_size..].to_vec()
}

/// DirectShow applications (OBS "Video Capture Device", FFmpeg) see a Media
/// Foundation camera through Frame Server's DirectShow bridge. For a width
/// that is not a multiple of 32 pixels both the native NV12 and the YUY2 the
/// source converts itself must arrive unsheared, byte for byte; the bridge's
/// own NV12 -> YUY2 conversion pads rows to 1088 and is never used.
#[test]
#[ignore = "requires the Media Foundation source installed machine-wide and FFmpeg on PATH"]
fn directshow_bridge_delivers_native_and_converted_modes_unsheared() {
    // SAFETY: Test thread setup.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-bridge-test").expect("app id"));
    let name = format!("VCRs MF Bridge {}", std::process::id());
    let (width, height) = (1080_usize, 1920_usize);
    let format = VideoFormat::new(PixelFormat::Nv12, width as u32, height as u32, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new(&name, format)).expect("register MF camera");
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager.clone(), info.id);

    // Position-dependent luma exposes any row shift; flat chroma keeps the
    // 4:2:0 -> 4:2:2 conversion exact.
    let luma = |x: usize, y: usize| (16 + (x * 7 + y * 13) % 220) as u8;
    let mut frame = vec![128_u8; width * height * 3 / 2];
    for y in 0..height {
        for x in 0..width {
            frame[y * width + x] = luma(x, y);
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let producer = {
        let (manager, frame, stop) = (manager.clone(), frame.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut camera = manager.open(info.id, format).expect("open producer");
            while !stop.load(Ordering::Relaxed) {
                camera.write_host(&frame).expect("write");
                std::thread::sleep(Duration::from_millis(33));
            }
        })
    };
    let (device, _) = video_devices().into_iter().find(|(device, _)| is_camera(device, &name)).expect("Frame Server lists the camera");

    let nv12 = directshow_capture(&device, "nv12", 30, frame.len());
    let yuy2 = directshow_capture(&device, "yuyv422", 30, width * height * 2);
    stop.store(true, Ordering::Relaxed);
    producer.join().expect("producer");

    assert!(nv12 == frame, "NV12 through the DirectShow bridge differs from the producer frame");
    let expected: Vec<u8> = (0..height).flat_map(|y| (0..width).flat_map(move |x| [luma(x, y), 128])).collect();
    assert!(yuy2 == expected, "YUY2 through the DirectShow bridge is not the source's tight conversion");
}
