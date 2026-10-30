//! End-to-end tests of the pure Rust DirectShow source through FFmpeg.
//!
//! Registers per-user cameras (no elevation), so it is opt-in:
//!
//! ```text
//! cargo test --test windows_dshow -- --ignored --test-threads=1
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use virtualcam::{AppId, DirectShowRegistration, Error, FrameRate, ManagedCameraId, ManagedCameraManager, PixelFormat, RegistrationHealth, VideoFormat, WriteStatus};

/// Builds the source DLL and copies it to a fresh per-run directory, so a
/// previous run's consumer can never hold the file we register.
fn staged_source() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "-p", "virtualcam-dshow", "--release"])
        .current_dir(&root)
        .status()
        .expect("run cargo build");
    assert!(status.success(), "build virtualcam-dshow");
    let directory = env::temp_dir().join(format!("VirtualCamRsTest-{}-{}", std::process::id(), Instant::now().elapsed().as_nanos()));
    fs::create_dir_all(&directory).expect("stage directory");
    let target = directory.join("virtualcam_dshow.dll");
    fs::copy(root.join("target/release/virtualcam_dshow.dll"), &target).expect("stage DLL");
    target
}

struct Registration {
    manager: ManagedCameraManager,
    id: ManagedCameraId,
    name: String,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = self.manager.unregister(self.id);
    }
}

fn register(source: &Path, app: &str, name: &str, default_format: Option<VideoFormat>) -> Registration {
    let manager = ManagedCameraManager::for_app(AppId::new(app).expect("app id")).with_dshow_source(source);
    let info = manager
        .register(DirectShowRegistration {
            default_format,
            ..DirectShowRegistration::new(name)
        })
        .expect("register per-user DirectShow camera");
    assert_eq!(info.health, RegistrationHealth::Healthy);
    Registration {
        manager,
        id: info.id,
        name: name.to_owned(),
    }
}

fn ffmpeg_devices() -> String {
    let output = Command::new("ffmpeg").args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]).output().expect("ffmpeg");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Captures the last of `frames` frames in the camera's negotiated format.
fn capture(name: &str, pix_fmt: &str, frames: u32) -> Vec<u8> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "dshow", "-i"])
        .arg(format!("video={name}"))
        .args(["-frames:v", &frames.to_string(), "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    // Drain both pipes concurrently; a full pipe would otherwise stall FFmpeg.
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut stderr, &mut text).map(|_| text)
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll ffmpeg") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("ffmpeg capture of {name} timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = out.join().expect("stdout thread").expect("read stdout");
    let stderr = err.join().expect("stderr thread").unwrap_or_default();
    assert!(status.success(), "ffmpeg capture failed: {stderr}");
    stdout
}

/// A static frame whose every byte depends on its position, so swapped rows,
/// planes, or padding mistakes all change the bytes.
fn pattern(format: &VideoFormat) -> Vec<u8> {
    let size = format.pixel_format.frame_size(format.width, format.height).expect("size");
    let mut frame: Vec<u8> = (0..size).map(|index| (index * 7 + index / 131) as u8).collect();
    if format.pixel_format == PixelFormat::P010 {
        // P010 keeps data in the high 10 bits; zero the low six bits.
        for pair in frame.chunks_exact_mut(2) {
            pair[0] &= 0xC0;
        }
    }
    if format.pixel_format == PixelFormat::Bgra32 {
        for pixel in frame.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
    frame
}

fn ffmpeg_pix_fmt(format: PixelFormat) -> &'static str {
    match format {
        PixelFormat::Nv12 => "nv12",
        PixelFormat::I420 => "yuv420p",
        PixelFormat::Yuy2 => "yuyv422",
        PixelFormat::Uyvy => "uyvy422",
        PixelFormat::P010 => "p010le",
        PixelFormat::Bgra32 => "bgra",
        PixelFormat::Rgb24 => "bgr24",
        other => panic!("{other:?} is not managed"),
    }
}

#[test]
#[ignore = "registers per-user DirectShow cameras and drives FFmpeg"]
fn every_managed_format_reaches_a_directshow_consumer_byte_exact() {
    let source = staged_source();
    let camera = register(&source, "dev.virtualcam.test", &format!("VCRs Format Test {}", std::process::id()), None);
    assert!(ffmpeg_devices().contains(&camera.name), "per-user camera is enumerated");

    // Odd byte widths exercise DIB row padding (RGB24 162 px = 486 bytes -> 488).
    let cases = [
        (PixelFormat::Nv12, 320, 240),
        (PixelFormat::I420, 320, 240),
        (PixelFormat::Yuy2, 320, 240),
        (PixelFormat::Uyvy, 320, 240),
        (PixelFormat::Bgra32, 160, 120),
        (PixelFormat::Rgb24, 162, 120),
        // P010 is negotiated too, but FFmpeg's dshow demuxer does not
        // implement that FourCC; its media type is covered by unit tests.
    ];
    for (pixel_format, width, height) in cases {
        let format = VideoFormat::new(pixel_format, width, height, FrameRate::fps(30));
        let frame = pattern(&format);
        let mut producer = camera.manager.open(camera.id, format).expect("open producer");
        assert_eq!(producer.video_format(), Some(format));
        assert_eq!(producer.write_host(&frame), Ok(WriteStatus::Written));
        let captured = capture(&camera.name, ffmpeg_pix_fmt(pixel_format), 3);
        assert_eq!(captured.len(), frame.len() * 3, "{pixel_format:?}: three frames at the producer's format");
        let last = &captured[frame.len() * 2..];
        if pixel_format == PixelFormat::Bgra32 {
            // DirectShow RGB32 carries no alpha guarantee; compare BGR only.
            let bgr = |data: &[u8]| data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect::<Vec<_>>();
            assert!(bgr(last) == bgr(&frame), "{pixel_format:?} frame differs");
        } else {
            assert!(last == frame.as_slice(), "{pixel_format:?} frame differs");
        }
        producer.close().expect("close producer");
    }
}

#[test]
#[ignore = "registers per-user DirectShow cameras and drives FFmpeg"]
fn idle_camera_uses_registered_default_then_history_and_returns_to_black_after_producer_crash() {
    if let Ok(id) = env::var("VIRTUALCAM_CRASH_HELPER_ID") {
        // Child mode: publish one frame, signal readiness, then die abruptly.
        let id: ManagedCameraId = id.parse().expect("id");
        let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.test").expect("app id"));
        let format = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        let mut camera = manager.open(id, format).expect("child producer");
        camera.write_host(&vec![200; camera.frame_size()]).expect("child frame");
        fs::write(env::var("VIRTUALCAM_CRASH_HELPER_SIGNAL").expect("signal path"), b"ready").expect("signal");
        thread::sleep(Duration::from_secs(60));
        std::process::abort();
    }

    let source = staged_source();
    let default = VideoFormat::new(PixelFormat::Yuy2, 96, 64, FrameRate::fps(15));
    let camera = register(&source, "dev.virtualcam.test", &format!("VCRs Crash Test {}", std::process::id()), Some(default));

    // No producer and no history: the registered default, in black.
    let black = capture(&camera.name, "yuyv422", 1);
    assert_eq!(black.len(), 96 * 64 * 2);
    assert!(black.chunks_exact(2).all(|pair| pair == [16, 128]), "idle YUY2 black");

    let signal = env::temp_dir().join(format!("VirtualCamRs-CrashReady-{}", std::process::id()));
    let _ = fs::remove_file(&signal);
    let mut child = Command::new(env::current_exe().expect("test exe"))
        .args(["--exact", "idle_camera_uses_registered_default_then_history_and_returns_to_black_after_producer_crash", "--ignored", "--nocapture"])
        .env("VIRTUALCAM_CRASH_HELPER_ID", camera.id.to_string())
        .env("VIRTUALCAM_CRASH_HELPER_SIGNAL", &signal)
        .spawn()
        .expect("spawn crash helper");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !signal.exists() {
        assert!(child.try_wait().expect("poll child").is_none(), "helper exited early");
        assert!(Instant::now() < deadline, "helper never published");
        thread::sleep(Duration::from_millis(20));
    }
    // Live producer frame at the producer's format.
    let live = capture(&camera.name, "nv12", 1);
    assert_eq!(live, vec![200; 64 * 48 * 3 / 2]);

    child.kill().expect("kill helper");
    child.wait().expect("reap helper");
    // The producer died without closing: consumers fall back to black at
    // the registered idle default, which outranks the user's history.
    let after = capture(&camera.name, "yuyv422", 1);
    assert_eq!(after.len(), 96 * 64 * 2);
    assert!(after.chunks_exact(2).all(|pair| pair == [16, 128]), "black after crash");
    let _ = fs::remove_file(&signal);

    // Without a registered default, the last published format is used.
    let undefaulted = register(&source, "dev.virtualcam.test", &format!("VCRs History Test {}", std::process::id()), None);
    let mut producer = undefaulted.manager.open(undefaulted.id, VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30))).expect("producer");
    producer.write_host(&vec![200; producer.frame_size()]).expect("frame");
    producer.close().expect("close");
    let idle = capture(&undefaulted.name, "nv12", 1);
    assert_eq!(idle.len(), 64 * 48 * 3 / 2, "history format");
}

#[test]
#[ignore = "registers per-user DirectShow cameras"]
fn app_scopes_isolate_registrations_and_unregister_removes_the_device() {
    let source = staged_source();
    let name = format!("VCRs Scope Test {}", std::process::id());
    let camera = register(&source, "dev.virtualcam.scope-a", &name, None);
    let other = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.scope-b").expect("app id")).with_dshow_source(&source);

    assert!(camera.manager.list().expect("list a").iter().any(|info| info.id == camera.id));
    assert!(other.list().expect("list b").iter().all(|info| info.id != camera.id));
    assert_eq!(other.get(camera.id), Err(Error::CameraNotFound));
    assert_eq!(other.unregister(camera.id), Err(Error::CameraNotFound));
    let global = ManagedCameraManager::new().get(camera.id).expect("global view sees every app");
    assert_eq!(global.app_id.as_ref().map(AppId::as_str), Some("dev.virtualcam.scope-a"));

    assert_eq!(camera.manager.register(DirectShowRegistration::new(name.to_uppercase())).map(|_| ()), Err(Error::NameConflict));
    let renamed = camera.manager.rename(camera.id, format!("{name} Renamed")).expect("rename");
    assert_eq!(renamed.name, format!("{name} Renamed"));
    assert!(ffmpeg_devices().contains(&renamed.name));

    camera.manager.unregister(camera.id).expect("unregister");
    assert_eq!(camera.manager.get(camera.id), Err(Error::CameraNotFound));
    assert!(!ffmpeg_devices().contains(&renamed.name), "device disappears");
}

/// Encodes `value` as 32 luma blocks (16x16, black/white) on the first rows.
fn stamp(frame: &mut [u8], width: usize, value: u32) {
    for bit in 0..32 {
        let luma = if value >> bit & 1 == 1 { 235 } else { 16 };
        for row in 0..16 {
            let start = row * width + bit * 16;
            frame[start..start + 16].fill(luma);
        }
    }
}

fn read_stamp(frame: &[u8], width: usize) -> u32 {
    (0..32).fold(0, |value, bit| value | (u32::from(frame[8 * width + bit * 16 + 8] > 128) << bit))
}

#[test]
#[ignore = "registers a per-user DirectShow camera and drives FFmpeg; prints latency"]
fn directshow_delivery_latency() {
    let source = staged_source();
    let camera = register(&source, "dev.virtualcam.test", &format!("VCRs Latency Test {}", std::process::id()), None);
    let parse = |key: &str, default: usize| env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let (width, height, fps) = (parse("LAT_W", 1920), parse("LAT_H", 1080), parse("LAT_FPS", 60) as u32);
    let format = VideoFormat::new(PixelFormat::Nv12, width as u32, height as u32, FrameRate::fps(fps));
    let mut producer = camera.manager.open(camera.id, format).expect("open producer");
    let base = Instant::now();
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let writer_running = running.clone();
    let writer = thread::spawn(move || {
        let mut frame = vec![128_u8; width * height * 3 / 2];
        frame[..width * height].fill(60);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        let mut next = Instant::now();
        while writer_running.load(std::sync::atomic::Ordering::Relaxed) {
            stamp(&mut frame, width, base.elapsed().as_micros() as u32);
            producer.write_host(&frame).expect("write");
            next += interval;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
        }
        producer.close().expect("close");
    });
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-threads", "1", "-f", "dshow", "-i"])
        .arg(format!("video={}", camera.name))
        .args(["-threads", "1", "-fps_mode", "passthrough", "-frames:v", "600", "-f", "rawvideo", "-pix_fmt", "nv12", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");
    let mut stdout = child.stdout.take().expect("stdout");
    let mut frame = vec![0_u8; width * height * 3 / 2];
    let mut latencies = Vec::new();
    let mut repeats = 0;
    let mut previous = None;
    while std::io::Read::read_exact(&mut stdout, &mut frame).is_ok() {
        let now = base.elapsed().as_micros() as u32;
        let written = read_stamp(&frame, width);
        if previous == Some(written) {
            repeats += 1;
        }
        previous = Some(written);
        latencies.push(f64::from(now.wrapping_sub(written)) / 1000.0);
    }
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    writer.join().expect("writer");
    let _ = child.wait();
    let steady = &latencies[latencies.len().min(60)..];
    let mut sorted = steady.to_vec();
    sorted.sort_by(f64::total_cmp);
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q) as usize];
    println!(
        "frames={} repeats={repeats} latency ms: min={:.1} p50={:.1} p90={:.1} max={:.1}",
        latencies.len(),
        pick(0.0),
        pick(0.5),
        pick(0.9),
        pick(1.0)
    );
    println!("first 10: {:?}", &latencies[..latencies.len().min(10)]);
    println!("last 10: {:?}", &latencies[latencies.len().saturating_sub(10)..]);
}

#[test]
#[ignore = "registers a per-user DirectShow camera and drives FFmpeg"]
fn consumer_requests_an_offered_frame_rate_and_the_source_paces_to_it() {
    let source = staged_source();
    let default = VideoFormat::new(PixelFormat::Nv12, 320, 240, FrameRate::fps(30));
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.test").expect("app id")).with_dshow_source(&source);
    let name = format!("VCRs Rate Test {}", std::process::id());
    let info = manager
        .register(DirectShowRegistration {
            default_format: Some(default),
            frame_rates: vec![FrameRate::fps(15), FrameRate::fps(60)],
            ..DirectShowRegistration::new(&name)
        })
        .expect("register");
    assert_eq!(info.frame_rates, [FrameRate::fps(15), FrameRate::fps(60)]);
    let camera = Registration { manager, id: info.id, name };

    // The consumer picks 15 FPS while the producer writes at about 60 FPS.
    let mut producer = camera.manager.open(camera.id, default).expect("open producer");
    let frames = 30_u32;
    let mut consumer = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "dshow", "-framerate", "15", "-i"])
        .arg(format!("video={}", camera.name))
        .args(["-frames:v", &frames.to_string(), "-f", "rawvideo", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg");
    let frame = pattern(&default);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut requested = None;
    let mut first_request = None;
    while consumer.try_wait().expect("poll ffmpeg").is_none() {
        producer.write_host(&frame).expect("write NV12");
        if requested.is_none() {
            requested = producer.requested_format();
            first_request = requested.map(|_| Instant::now());
        }
        assert!(Instant::now() < deadline, "consumer never finished");
        thread::sleep(Duration::from_millis(16));
    }
    let elapsed = first_request.expect("consumer published its format").elapsed();
    let output = consumer.wait_with_output().expect("ffmpeg");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let requested = requested.expect("requested format");
    assert_eq!((requested.pixel_format, requested.frame_rate.interval_100ns()), (PixelFormat::Nv12, Some(666_666)));
    // 30 frames at 15 FPS take about two seconds, not the half second at 60 FPS.
    assert!(elapsed >= Duration::from_millis(1500), "delivery was not paced to 15 FPS: {elapsed:?}");
    producer.close().expect("close");
}

#[test]
#[ignore = "registers a per-user DirectShow camera and drives FFmpeg"]
fn single_consumer_requests_its_pixel_format_from_the_producer() {
    let source = staged_source();
    let default = VideoFormat::new(PixelFormat::Nv12, 320, 240, FrameRate::fps(30));
    let manager = ManagedCameraManager::for_app(AppId::new("dev.virtualcam.test").expect("app id")).with_dshow_source(&source);
    let name = format!("VCRs Request Test {}", std::process::id());
    let info = manager
        .register(DirectShowRegistration {
            default_format: Some(default),
            pixel_formats: vec![PixelFormat::Nv12, PixelFormat::Yuy2, PixelFormat::I420],
            ..DirectShowRegistration::new(&name)
        })
        .expect("register");
    assert_eq!(info.pixel_formats, [PixelFormat::Nv12, PixelFormat::Yuy2, PixelFormat::I420]);
    let camera = Registration { manager, id: info.id, name };

    let spawn = |frames: u32| {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-f", "dshow", "-pixel_format", "yuyv422", "-i"])
            .arg(format!("video={}", camera.name))
            .args(["-frames:v", &frames.to_string(), "-f", "rawvideo", "-pix_fmt", "yuyv422", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ffmpeg")
    };

    // The producer starts in the default NV12; the consumer picks YUY2.
    let mut producer = camera.manager.open(camera.id, default).expect("open producer");
    let mut first = spawn(90);
    let mut stdout = first.stdout.take().expect("stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let requested = loop {
        producer.write_host(&vec![16; producer.frame_size()]).expect("write NV12");
        if let Some(format) = producer.requested_format() {
            break format;
        }
        assert!(Instant::now() < deadline, "consumer never published its format");
        thread::sleep(Duration::from_millis(30));
    };
    assert_eq!((requested.pixel_format, requested.width, requested.height), (PixelFormat::Yuy2, 320, 240));

    // A second graph is refused while the first holds the lease.
    let second = spawn(1).wait_with_output().expect("second consumer");
    assert!(!second.status.success(), "second consumer must be refused");

    // The producer switches to the requested format and real frames flow.
    producer.close().expect("close NV12 producer");
    drop(producer);
    let yuy2 = VideoFormat::new(PixelFormat::Yuy2, 320, 240, FrameRate::fps(30));
    let mut producer = camera.manager.open(camera.id, yuy2).expect("open YUY2 producer");
    let frame = pattern(&yuy2);
    while first.try_wait().expect("poll ffmpeg").is_none() {
        producer.write_host(&frame).expect("write YUY2");
        assert!(Instant::now() < deadline + Duration::from_secs(15), "first consumer never finished");
        thread::sleep(Duration::from_millis(30));
    }
    let captured = reader.join().expect("reader").expect("read stdout");
    assert_eq!(captured.len(), frame.len() * 90);
    assert!(&captured[frame.len() * 89..] == frame.as_slice(), "last frame is the producer's YUY2 frame");
    // Disconnecting releases the lease and clears the request.
    let deadline = Instant::now() + Duration::from_secs(5);
    while producer.requested_format().is_some() {
        assert!(Instant::now() < deadline, "request was not cleared");
        thread::sleep(Duration::from_millis(30));
    }
    producer.close().expect("close");
}
