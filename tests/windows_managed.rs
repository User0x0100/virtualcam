#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use virtualcam::{CameraType, Error, ManagedCameraConfig, ManagedCameraId, ManagedCameraManager, ManagedStreamConfig, ObsFrameRate, PixelFormat, VirtualCamera, WriteStatus};

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
        frame_rate: ObsFrameRate { numerator: fps, denominator: 1 },
    }
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
#[ignore = "requires elevation, a built virtualcam-managed.dll, and ffmpeg"]
fn managed_driver_registers_enumerates_and_streams_runtime_formats() {
    let source_driver = PathBuf::from(std::env::var_os("VIRTUALCAM_MANAGED_DRIVER").expect("set VIRTUALCAM_MANAGED_DRIVER to virtualcam-managed.dll"));
    let program_files = PathBuf::from(std::env::var_os("ProgramFiles").expect("ProgramFiles must be defined"));
    let canonical_source = source_driver.canonicalize().expect("canonicalize source managed driver");
    let canonical_program_files = program_files.canonicalize().expect("canonicalize Program Files");

    if !canonical_source.starts_with(&canonical_program_files) {
        let unsafe_manager = ManagedCameraManager::new(&source_driver);
        let error = unsafe_manager
            .register(ManagedCameraConfig {
                name: format!("VirtualCam Security Probe {}", std::process::id()),
            })
            .expect_err("machine-wide COM registration must reject a user-writable driver location");
        assert!(matches!(error, Error::Hresult { code, .. } if code as u32 == 0x8007_0005));
    }

    let install_directory = program_files.join(format!("VirtualCamRs-Integration-{}", std::process::id()));
    fs::create_dir_all(&install_directory).expect("create protected integration driver directory");
    let _install_guard = InstalledDriverGuard { directory: install_directory.clone() };
    let installed_driver = install_directory.join("virtualcam-managed.dll");
    fs::copy(&source_driver, &installed_driver).expect("stage managed driver under Program Files");
    let manager = ManagedCameraManager::new(&installed_driver);

    let name = format!("VirtualCam NV12 Integration {}", std::process::id());
    let info = manager.register(ManagedCameraConfig { name: name.clone() }).expect("register managed camera");
    let guard = RegistrationGuard { manager: manager.clone(), id: info.id };

    // `open` must validate the DLL that COM will actually load, not merely the
    // manager's own DLL path. Temporarily hide InprocServer32's target while a
    // separate v2 manager remains loadable from the build directory.
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
    assert_eq!(landscape.camera_type(), CameraType::ManagedNv12);
    assert_eq!(landscape.native_format(), PixelFormat::Nv12);
    assert_eq!(landscape.frame_size(), 320 * 240 * 3 / 2);
    capture_with_ffmpeg(&renamed, 320, 240, 30, &mut landscape);
    landscape.close().expect("close landscape producer");

    let same_registration = manager.get(info.id).expect("registration survives stream close");
    assert_eq!(same_registration.id, info.id);
    assert_eq!(same_registration.name, renamed);

    let mut portrait = manager.open(info.id, stream(240, 320, 25)).expect("reopen same CLSID at portrait runtime format");
    assert_eq!(portrait.frame_size(), 240 * 320 * 3 / 2);
    capture_with_ffmpeg(&renamed, 240, 320, 25, &mut portrait);
    portrait.close().expect("close portrait producer");

    manager.unregister(info.id).expect("unregister managed camera");
    manager.unregister(second.id).expect("unregister second managed camera");
    std::mem::forget(guard);
    std::mem::forget(second_guard);
}
