#![cfg(windows)]

use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use virtualcam::{CameraType, Error, FrameRate, ManagedCameraConfig, ManagedCameraId, ManagedCameraManager, ManagedStreamConfig, PixelFormat, VirtualCamera, WriteStatus};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW};

struct RegistrationGuard {
    manager: ManagedCameraManager,
    id: ManagedCameraId,
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        let _ = self.manager.unregister(self.id);
    }
}

struct InstalledDriverGuard {
    directory: PathBuf,
}

impl Drop for InstalledDriverGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct MovedDriverGuard {
    original: PathBuf,
    moved: PathBuf,
}

impl Drop for MovedDriverGuard {
    fn drop(&mut self) {
        if self.moved.exists() {
            let _ = fs::rename(&self.moved, &self.original);
        }
    }
}

fn stream(width: u32, height: u32, fps: u32) -> ManagedStreamConfig {
    ManagedStreamConfig {
        width,
        height,
        frame_rate: FrameRate { numerator: fps, denominator: 1 },
    }
}

fn registered_inproc_server(id: ManagedCameraId) -> PathBuf {
    let mut key_path: Vec<u16> = format!("CLSID\\{id}\\InprocServer32").encode_utf16().collect();
    key_path.push(0);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `key_path` is NUL-terminated and `key` points to writable storage.
    let status = unsafe { RegOpenKeyExW(HKEY_CLASSES_ROOT, key_path.as_ptr(), 0, KEY_READ, &mut key) };
    assert_eq!(status, 0, "open managed InprocServer32");

    let mut value_type = 0_u32;
    let mut bytes = 0_u32;
    // SAFETY: A null value name queries the default value; this call requests size only.
    let status = unsafe { RegQueryValueExW(key, std::ptr::null(), std::ptr::null(), &mut value_type, std::ptr::null_mut(), &mut bytes) };
    assert_eq!(status, 0, "query managed InprocServer32 size");
    assert_eq!(value_type, REG_SZ);
    assert!(bytes >= 2 && bytes.is_multiple_of(2));

    let mut buffer = vec![0_u16; bytes as usize / 2];
    // SAFETY: `buffer` has the byte capacity reported by the preceding registry query.
    let status = unsafe { RegQueryValueExW(key, std::ptr::null(), std::ptr::null(), &mut value_type, buffer.as_mut_ptr().cast::<u8>(), &mut bytes) };
    // SAFETY: Successful RegOpenKeyExW returned one owned registry key handle.
    unsafe { RegCloseKey(key) };
    assert_eq!(status, 0, "read managed InprocServer32");
    assert_eq!(value_type, REG_SZ);

    let end = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
    PathBuf::from(OsString::from_wide(&buffer[..end]))
}

fn capture_without_producer(name: &str, width: u32, height: u32, fps: u32) {
    let output = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-f")
        .arg("dshow")
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-framerate")
        .arg(fps.to_string())
        .arg("-i")
        .arg(format!("video={name}"))
        .arg("-frames:v")
        .arg("1")
        .arg("-pix_fmt")
        .arg("nv12")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("capture managed fallback frame without producer");
    assert!(
        output.status.success(),
        "ffmpeg could not open idle managed camera at {width}x{height}@{fps}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let y_size = width as usize * height as usize;
    assert_eq!(output.stdout.len(), y_size * 3 / 2, "fallback NV12 frame size");
    assert!(output.stdout[..y_size].iter().all(|value| *value == 16), "fallback Y plane must be limited-range black");
    assert!(output.stdout[y_size..].iter().all(|value| *value == 128), "fallback UV plane must be neutral chroma");
}

fn assert_consumer_recovers_to_black_after_producer_crash(name: &str, id: ManagedCameraId, driver: &PathBuf) {
    let width = 160_u32;
    let height = 120_u32;
    let fps = 30_u32;
    let frame_size = width as usize * height as usize * 3 / 2;
    let signal = std::env::temp_dir().join(format!("VirtualCamRs-CrashReady-{}", std::process::id()));
    let _ = fs::remove_file(&signal);

    let mut producer = Command::new(std::env::current_exe().expect("current managed integration executable"))
        .args(["--exact", "managed_crash_producer_helper", "--nocapture"])
        .env("VIRTUALCAM_CRASH_HELPER_ID", id.to_string())
        .env("VIRTUALCAM_CRASH_HELPER_DRIVER", driver)
        .env("VIRTUALCAM_CRASH_HELPER_SIGNAL", &signal)
        .spawn()
        .expect("spawn crash producer helper");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !signal.exists() {
        if let Some(status) = producer.try_wait().expect("poll crash producer helper") {
            panic!("crash producer helper exited before publishing a frame: {status}");
        }
        assert!(Instant::now() < deadline, "crash producer helper did not become ready");
        thread::sleep(Duration::from_millis(20));
    }

    let consumer = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-f")
        .arg("dshow")
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-framerate")
        .arg(fps.to_string())
        .arg("-i")
        .arg(format!("video={name}"))
        .arg("-frames:v")
        .arg("45")
        .arg("-pix_fmt")
        .arg("nv12")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn crash-recovery DirectShow consumer");

    // Let the consumer bind while the colored producer session is definitely
    // live, then terminate the process without running VirtualCamera::close.
    thread::sleep(Duration::from_millis(350));
    producer.kill().expect("terminate crash producer helper");
    let _ = producer.wait();
    let _ = fs::remove_file(&signal);

    let output = consumer.wait_with_output().expect("collect crash-recovery consumer");
    assert!(output.status.success(), "crash-recovery ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout.len(), frame_size * 45, "crash-recovery raw frame count");

    let first = &output.stdout[..frame_size];
    let last = &output.stdout[output.stdout.len() - frame_size..];
    let y_size = width as usize * height as usize;
    assert!(first[..y_size].iter().all(|value| *value == 96), "consumer should initially see the producer's colored Y plane");
    assert!(last[..y_size].iter().all(|value| *value == 16), "consumer should switch to black after the producer lease disappears");
    assert!(last[y_size..].iter().all(|value| *value == 128), "crash fallback UV plane must be neutral chroma");
}

fn capture_with_ffmpeg(name: &str, width: u32, height: u32, fps: u32, camera: &mut VirtualCamera) {
    let mut child = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-f")
        .arg("dshow")
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-framerate")
        .arg(fps.to_string())
        .arg("-i")
        .arg(format!("video={name}"))
        .arg("-frames:v")
        .arg("3")
        .arg("-f")
        .arg("null")
        .arg("-")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg capture");

    let y_size = width as usize * height as usize;
    let mut frame = vec![0_u8; camera.frame_size()];
    frame[..y_size].fill(96);
    frame[y_size..].fill(128);

    let mut exited = false;
    for _ in 0..120 {
        assert_eq!(camera.write_host(&frame).expect("publish NV12 frame"), WriteStatus::Written);
        if child.try_wait().expect("poll ffmpeg").is_some() {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(12));
    }

    if !exited {
        child.kill().expect("kill stalled ffmpeg");
    }
    let output = child.wait_with_output().expect("collect ffmpeg output");
    assert!(exited && output.status.success(), "ffmpeg capture failed for {width}x{height}@{fps}: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn managed_crash_producer_helper() {
    let Some(id) = std::env::var_os("VIRTUALCAM_CRASH_HELPER_ID") else {
        return;
    };
    let driver = PathBuf::from(std::env::var_os("VIRTUALCAM_CRASH_HELPER_DRIVER").expect("crash helper driver path"));
    let signal = PathBuf::from(std::env::var_os("VIRTUALCAM_CRASH_HELPER_SIGNAL").expect("crash helper signal path"));
    let id: ManagedCameraId = id.to_string_lossy().parse().expect("crash helper camera id");
    let manager = ManagedCameraManager::new(driver);
    let mut camera = manager.open(id, stream(160, 120, 30)).expect("crash helper opens managed producer");
    let y_size = 160 * 120;
    let mut frame = vec![0_u8; camera.frame_size()];
    frame[..y_size].fill(96);
    frame[y_size..].fill(128);
    assert_eq!(camera.write_host(&frame).expect("crash helper publishes frame"), WriteStatus::Written);
    fs::write(signal, b"ready").expect("signal crash helper readiness");
    thread::sleep(Duration::from_secs(60));
}

#[test]
#[ignore = "requires elevation, a built virtualcam-managed.dll, and ffmpeg"]
fn managed_driver_registers_enumerates_and_streams_runtime_formats() {
    let source_driver = PathBuf::from(std::env::var_os("VIRTUALCAM_MANAGED_DRIVER").expect("set VIRTUALCAM_MANAGED_DRIVER to virtualcam-managed.dll"));

    // Stage the driver in an ordinary app-local directory outside Program Files.
    // Managed cameras intentionally support portable applications: registration
    // must persist the canonical DLL beside the owning app rather than require a
    // machine-wide installation directory.
    let install_directory = std::env::temp_dir().join(format!("VirtualCamRs-Integration-{}", std::process::id()));
    fs::create_dir_all(&install_directory).expect("create app-local integration driver directory");
    let _install_guard = InstalledDriverGuard { directory: install_directory.clone() };
    let installed_driver = install_directory.join("virtualcam-managed.dll");
    fs::copy(&source_driver, &installed_driver).expect("stage app-local managed driver");
    let manager = ManagedCameraManager::new(&installed_driver);

    let name = format!("VirtualCam NV12 Integration {}", std::process::id());
    let info = manager.register(ManagedCameraConfig { name: name.clone() }).expect("register app-local managed camera");
    let guard = RegistrationGuard { manager: manager.clone(), id: info.id };
    let registered_driver = registered_inproc_server(info.id);
    let registered_text = registered_driver.to_string_lossy();
    assert!(!registered_text.starts_with(r"\\?\"), "InprocServer32 must use a normal DOS path, got {registered_text}");
    assert_eq!(registered_driver, installed_driver, "InprocServer32 should persist the app-local DLL path");

    // `open` must validate the DLL that COM will actually load, not merely the
    // manager's own DLL path. Temporarily hide InprocServer32's target while a
    // separate v3 manager remains loadable from the build directory.
    let moved_driver = install_directory.join("virtualcam-managed.dll.moved");
    fs::rename(&installed_driver, &moved_driver).expect("temporarily move registered driver");
    let moved_guard = MovedDriverGuard {
        original: installed_driver.clone(),
        moved: moved_driver,
    };
    let source_manager = ManagedCameraManager::new(&source_driver);
    let open_error = match source_manager.open(info.id, stream(80, 80, 20)) {
        Ok(mut camera) => {
            let _ = camera.close();
            panic!("open must fail while the registered InprocServer32 target is unavailable");
        }
        Err(error) => error,
    };
    assert!(matches!(
        open_error,
        Error::Windows {
            operation: "LoadLibraryW(virtualcam-managed.dll)",
            ..
        }
    ));
    drop(moved_guard);

    let second_name = format!("VirtualCam NV12 Integration B {}", std::process::id());
    let second = manager.register(ManagedCameraConfig { name: second_name.clone() }).expect("register second managed camera");
    let second_guard = RegistrationGuard { manager: manager.clone(), id: second.id };

    assert_eq!(manager.get(info.id).expect("get registration"), info);
    let renamed = format!("{name} Renamed");
    manager.rename(info.id, &renamed).expect("rename managed camera");
    assert_eq!(manager.get(info.id).expect("get renamed registration").name, renamed);

    // A registered camera must remain a usable DirectShow capture device even
    // with no Rust producer. The native filter advertises the conservative
    // 640x480@30 NV12 fallback and emits black frames in this state.
    capture_without_producer(&renamed, 640, 480, 30);

    assert_consumer_recovers_to_black_after_producer_crash(&renamed, info.id, &installed_driver);
    // Even an abnormally terminated producer leaves its last successful mode
    // available to a newly opened DirectShow consumer.
    capture_without_producer(&renamed, 160, 120, 30);

    let listed = manager.list().expect("list registrations");
    assert!(listed.iter().any(|camera| camera.id == info.id));
    assert!(listed.iter().any(|camera| camera.id == second.id));

    let mut second_camera = manager.open(second.id, stream(160, 120, 24)).expect("open second independent producer");
    let second_frame = vec![128_u8; second_camera.frame_size()];
    assert_eq!(second_camera.write_host(&second_frame).expect("publish second camera frame"), WriteStatus::Written);
    second_camera.close().expect("close second producer");

    let enumeration = Command::new("ffmpeg")
        .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .output()
        .expect("run ffmpeg DirectShow enumeration");
    let enumeration_text = String::from_utf8_lossy(&enumeration.stderr);
    assert!(enumeration_text.contains(&renamed), "DirectShow enumeration did not contain {renamed}:\n{enumeration_text}");
    assert!(enumeration_text.contains(&second_name), "DirectShow enumeration did not contain {second_name}:\n{enumeration_text}");

    let mut landscape = manager.open(info.id, stream(320, 240, 30)).expect("open managed producer at landscape runtime format");
    assert_eq!(landscape.camera_type(), CameraType::ManagedDirectShow);
    assert_eq!(landscape.native_format(), PixelFormat::Nv12);
    assert_eq!(landscape.frame_size(), 320 * 240 * 3 / 2);
    capture_with_ffmpeg(&renamed, 320, 240, 30, &mut landscape);
    landscape.close().expect("close landscape producer");
    capture_without_producer(&renamed, 320, 240, 30);

    let same_registration = manager.get(info.id).expect("registration survives stream close");
    assert_eq!(same_registration.id, info.id);
    assert_eq!(same_registration.name, renamed);

    let mut portrait = manager.open(info.id, stream(240, 320, 25)).expect("reopen same CLSID at portrait runtime format");
    assert_eq!(portrait.frame_size(), 240 * 320 * 3 / 2);
    capture_with_ffmpeg(&renamed, 240, 320, 25, &mut portrait);
    portrait.close().expect("close portrait producer");
    capture_without_producer(&renamed, 240, 320, 25);

    manager.unregister(info.id).expect("unregister managed camera");
    manager.unregister(second.id).expect("unregister second managed camera");
    std::mem::forget(guard);
    std::mem::forget(second_guard);
}
