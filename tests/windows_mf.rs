//! End-to-end tests of a real Frame Server virtual camera.
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

use virtualcam::{AppId, FrameRate, ManagedCameraId, ManagedCameraManager, MediaFoundationRegistration, PixelFormat, RegistrationHealth, VideoFormat, WriteStatus};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaSource, IMFSample, IMFSourceReader, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_SUBTYPE, MF_VERSION,
    MFCreateAttributes, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFSTARTUP_FULL, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows_core::{GUID, PWSTR};

const FIRST_VIDEO_STREAM: u32 = 0xFFFF_FFFC;
/// Every format a camera may declare, in declaration order.
const FIVE: [PixelFormat; 5] = [PixelFormat::Nv12, PixelFormat::I420, PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::P010];
/// The same set in descriptor order: packed 4:2:2 before planar 4:2:0.
const ORDERED: [PixelFormat; 5] = [PixelFormat::Yuy2, PixelFormat::Uyvy, PixelFormat::Nv12, PixelFormat::I420, PixelFormat::P010];

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

/// A test pattern whose luma depends on the position and whose chroma is flat:
/// any consumer-side resampling keeps it byte-identical, so a mismatch means
/// the wrong frame (or a sheared one) arrived.
fn pattern(format: &VideoFormat) -> Vec<u8> {
    let (width, height) = (format.width as usize, format.height as usize);
    let luma = |x: usize, y: usize| (16 + (x * 7 + y * 13) % 220) as u8;
    let mut frame = vec![0_u8; format.pixel_format.layout(format.width, format.height).expect("layout").size()];
    match format.pixel_format {
        PixelFormat::Nv12 | PixelFormat::I420 => {
            for y in 0..height {
                for x in 0..width {
                    frame[y * width + x] = luma(x, y);
                }
            }
            frame[width * height..].fill(128);
        }
        PixelFormat::Yuy2 | PixelFormat::Uyvy => {
            let packed_yuy2 = format.pixel_format == PixelFormat::Yuy2;
            for y in 0..height {
                for x in (0..width).step_by(2) {
                    let (y0, y1) = (luma(x, y), luma(x + 1, y));
                    let bytes = if packed_yuy2 { [y0, 128, y1, 128] } else { [128, y0, 128, y1] };
                    let start = (y * width + x) * 2;
                    frame[start..start + 4].copy_from_slice(&bytes);
                }
            }
        }
        PixelFormat::P010 => {
            for y in 0..height {
                for x in 0..width {
                    let value = u16::from(luma(x, y)) << 6;
                    frame[(y * width + x) * 2..][..2].copy_from_slice(&value.to_le_bytes());
                }
            }
            for chunk in frame[width * height * 2..].chunks_exact_mut(2) {
                chunk.copy_from_slice(&(512_u16 << 6).to_le_bytes());
            }
        }
        other => panic!("no pattern for {other:?}"),
    }
    frame
}

fn subtype(format: PixelFormat) -> GUID {
    GUID::from_u128(format.mf_subtype().expect("subtype"))
}

/// The media types the source offers, in enumeration order.
fn offered_modes(reader: &IMFSourceReader) -> Vec<PixelFormat> {
    (0..)
        .map_while(|index| {
            // SAFETY: Valid reader; a missing index ends the enumeration.
            unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index) }.ok()
        })
        .map(|media_type| {
            // SAFETY: Valid media type.
            PixelFormat::from_mf_subtype(unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.expect("subtype").to_u128()).expect("managed subtype")
        })
        .collect()
}

/// Selects `pixel_format` on the reader, as a consumer switching modes does.
fn select(reader: &IMFSourceReader, pixel_format: PixelFormat) {
    let media_type = (0..)
        .map_while(|index| {
            // SAFETY: Valid reader; a missing index ends the enumeration.
            unsafe { reader.GetNativeMediaType(FIRST_VIDEO_STREAM, index) }.ok()
        })
        // SAFETY: Valid media type.
        .find(|media_type| unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|guid| guid == subtype(pixel_format)))
        .expect("declared type offered");
    // SAFETY: An offered media type on a live reader.
    unsafe { reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &media_type) }.expect("select");
}

/// Reads samples until one equals `expected`, or the deadline passes.
fn read_until(reader: &IMFSourceReader, expected: &[u8]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut sample: Option<IMFSample> = None;
        let mut flags = 0_u32;
        // SAFETY: Synchronous read into valid outputs.
        unsafe { reader.ReadSample(FIRST_VIDEO_STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)) }.expect("ReadSample");
        let Some(sample) = sample else { continue };
        // SAFETY: Lock/unlock pair on the sample's own buffer.
        unsafe {
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
}

/// Opens the system camera `name` through Frame Server.
fn open_camera(name: &str) -> (IMFMediaSource, IMFSourceReader) {
    let (_, device) = video_devices().into_iter().find(|(device, _)| is_camera(device, name)).expect("Frame Server lists the camera");
    // SAFETY: System-enumerated device; both objects live for the test.
    unsafe {
        let source = device.ActivateObject::<IMFMediaSource>().expect("activate");
        let reader = MFCreateSourceReaderFromMediaSource(&source, None).expect("reader");
        (source, reader)
    }
}

/// Runs a producer that reopens whenever consumers select another mode, so
/// the source can copy every frame it writes unchanged.
fn spawn_producer(manager: ManagedCameraManager, id: ManagedCameraId, format: VideoFormat, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut camera = manager.open(id, format).expect("open producer");
        let mut written = 0_u64;
        while !stop.load(Ordering::Relaxed) {
            if let Some(wanted) = camera.requested_format() {
                camera.close().expect("close");
                camera = manager.open(id, wanted).expect("reopen in the selected mode");
            }
            let frame = pattern(&camera.video_format().expect("format"));
            if camera.write_host(&frame).expect("write") == WriteStatus::Written {
                written += 1;
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        written
    })
}

#[test]
#[ignore = "requires the installed MF source and a real Frame Server clock"]
fn frame_server_idle_delivery_follows_default_rate() {
    // SAFETY: Initializes COM/MF for this system-device test.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-test").expect("app id"));
    let name = format!("VCRs MF Idle Rate {}", std::process::id());
    let format = VideoFormat::new(PixelFormat::Nv12, 640, 360, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::single(&name, format)).expect("register");
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager, info.id);
    let (source, reader) = open_camera(&name);
    // SAFETY: System source; only reads its black frames, no producer.
    unsafe {
        let read = || {
            let (mut sample, mut flags) = (None::<IMFSample>, 0);
            reader.ReadSample(FIRST_VIDEO_STREAM, 0, None, Some(&mut flags), None, Some(&mut sample)).expect("read");
            sample.is_some()
        };
        while !read() {}
        let started = Instant::now();
        let mut frames = 0;
        while started.elapsed() < Duration::from_secs(2) {
            frames += u32::from(read());
        }
        let fps = f64::from(frames) / started.elapsed().as_secs_f64();
        println!("idle delivery: {fps:.2} FPS");
        let _ = source.Shutdown();
        assert!((27.0..=33.0).contains(&fps), "idle 30 FPS camera delivered {fps:.2} FPS; coarse wait timeouts round to ~21 FPS in Session 0");
    }
}

/// A camera declaring all five YUV formats enumerates them, follows the
/// consumer's selection through every declared mode, and delivers each
/// producer frame unchanged.
#[test]
#[ignore = "requires the Media Foundation source installed machine-wide (elevated, once)"]
fn frame_server_camera_follows_every_declared_mode() {
    // SAFETY: Test thread setup.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    assert!(ManagedCameraManager::media_foundation_source().is_some(), "install the Media Foundation source first");
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-test").expect("app id"));
    let name = format!("VCRs MF Test {}", std::process::id());
    let default = VideoFormat::new(PixelFormat::Nv12, 640, 360, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new(&name, default, FIVE)).expect("register MF camera");
    assert_eq!(info.health, RegistrationHealth::Healthy);
    assert_eq!(info.pixel_formats, FIVE, "the declared set is recorded in declaration order");
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager.clone(), info.id);

    let stop = Arc::new(AtomicBool::new(false));
    let producer = spawn_producer(manager.clone(), info.id, default, Arc::clone(&stop));
    let (source, reader) = open_camera(&name);

    assert_eq!(offered_modes(&reader), ORDERED, "Frame Server offers every declared format, packed 4:2:2 first");
    // Frame Server makes the first listed type the default; the producer
    // follows it, so its frames arrive unchanged.
    assert!(
        read_until(
            &reader,
            &pattern(&VideoFormat {
                pixel_format: PixelFormat::Yuy2,
                ..default
            })
        ),
        "the default (first listed) mode arrives"
    );
    for selected in [PixelFormat::I420, PixelFormat::Uyvy, PixelFormat::P010, PixelFormat::Nv12, PixelFormat::Yuy2] {
        select(&reader, selected);
        let expected = pattern(&VideoFormat { pixel_format: selected, ..default });
        assert!(read_until(&reader, &expected), "{selected:?}: switched mode never arrived unchanged");
    }
    stop.store(true, Ordering::Relaxed);
    let written = producer.join().expect("producer");
    // SAFETY: Valid source.
    let _ = unsafe { source.Shutdown() };
    assert!(written > 0, "producer never attached to Frame Server");

    manager.unregister(info.id).expect("unregister");
    assert!(video_devices().iter().all(|(device, _)| !is_camera(device, &name)), "camera removed");
}

/// A consumer that never starts a producer still enumerates and can select
/// every declared mode; it receives black at the declared rate.
#[test]
#[ignore = "requires the Media Foundation source installed machine-wide (elevated, once)"]
fn frame_server_camera_without_producer_offers_every_mode() {
    // SAFETY: Test thread setup.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-test").expect("app id"));
    let name = format!("VCRs MF Idle {}", std::process::id());
    let default = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new(&name, default, FIVE)).expect("register");
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager, info.id);
    let (source, reader) = open_camera(&name);
    assert_eq!(offered_modes(&reader), ORDERED);
    for selected in ORDERED {
        select(&reader, selected);
        let format = VideoFormat { pixel_format: selected, ..default };
        let mut expected = vec![0_u8; format.pixel_format.layout(64, 48).expect("layout").size()];
        virtualcam_protocol::planes::fill_black(&mut expected, &format.pixel_format.layout(64, 48).expect("layout"), selected, format.color.is_full_range(selected)).expect("black");
        assert!(read_until(&reader, &expected), "{selected:?}: no black frame without a producer");
    }
    // SAFETY: Valid source.
    let _ = unsafe { source.Shutdown() };
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
/// that is not a multiple of 32 pixels both the native NV12 and the declared
/// YUY2 must arrive unsheared, byte for byte: the source lists 4:2:2 first, so
/// the bridge passes both through instead of using its own row-padding
/// NV12 -> YUY2 conversion.
#[test]
#[ignore = "requires the Media Foundation source installed machine-wide and FFmpeg on PATH"]
fn directshow_bridge_delivers_declared_modes_unsheared() {
    // SAFETY: Test thread setup.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup");
    }
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.mf-bridge-test").expect("app id"));
    let name = format!("VCRs MF Bridge {}", std::process::id());
    let format = VideoFormat::new(PixelFormat::Nv12, 1080, 1920, FrameRate::fps(30));
    let info = manager.register(MediaFoundationRegistration::new(&name, format, FIVE)).expect("register MF camera");
    struct Cleanup(ManagedCameraManager, virtualcam::ManagedCameraId);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.unregister(self.1);
        }
    }
    let _cleanup = Cleanup(manager.clone(), info.id);

    let stop = Arc::new(AtomicBool::new(false));
    let producer = spawn_producer(manager.clone(), info.id, format, Arc::clone(&stop));
    let (device, _) = video_devices().into_iter().find(|(device, _)| is_camera(device, &name)).expect("Frame Server lists the camera");

    let nv12 = directshow_capture(&device, "nv12", 30, pattern(&format).len());
    let yuy2 = directshow_capture(&device, "yuyv422", 30, pattern(&VideoFormat { pixel_format: PixelFormat::Yuy2, ..format }).len());
    stop.store(true, Ordering::Relaxed);
    producer.join().expect("producer");

    assert!(nv12 == pattern(&format), "NV12 through the DirectShow bridge differs from the producer frame");
    assert!(
        yuy2 == pattern(&VideoFormat { pixel_format: PixelFormat::Yuy2, ..format }),
        "YUY2 through the DirectShow bridge is not the producer's frame"
    );
}
