#![cfg(windows)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use virtualcam::{CameraType, Error, ManagedCameraConfig, ManagedCameraId, ManagedCameraManager, ObsFrameRate, PixelFormat, WriteStatus};

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

#[test]
#[ignore = "requires elevation, a built virtualcam-managed.dll, and ffmpeg"]
fn managed_driver_registers_enumerates_and_streams_nv12() {
    let source_driver = PathBuf::from(std::env::var_os("VIRTUALCAM_MANAGED_DRIVER").expect("set VIRTUALCAM_MANAGED_DRIVER to virtualcam-managed.dll"));
    let program_files = PathBuf::from(std::env::var_os("ProgramFiles").expect("ProgramFiles must be defined"));
    let canonical_source = source_driver.canonicalize().expect("canonicalize source managed driver");
    let canonical_program_files = program_files.canonicalize().expect("canonicalize Program Files");

    if !canonical_source.starts_with(&canonical_program_files) {
        let unsafe_manager = ManagedCameraManager::new(&source_driver);
        let error = unsafe_manager
            .register(ManagedCameraConfig {
                name: format!("VirtualCam Security Probe {}", std::process::id()),
                width: 160,
                height: 120,
                frame_rate: ObsFrameRate { numerator: 30, denominator: 1 },
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
    let info = manager
        .register(ManagedCameraConfig {
            name: name.clone(),
            width: 320,
            height: 240,
            frame_rate: ObsFrameRate { numerator: 30, denominator: 1 },
        })
        .expect("register managed camera");
    let guard = RegistrationGuard { manager: manager.clone(), id: info.id };
    let second_name = format!("VirtualCam NV12 Integration B {}", std::process::id());
    let second = manager
        .register(ManagedCameraConfig {
            name: second_name.clone(),
            width: 160,
            height: 120,
            frame_rate: ObsFrameRate { numerator: 24, denominator: 1 },
        })
        .expect("register second managed camera");
    let second_guard = RegistrationGuard { manager: manager.clone(), id: second.id };

    assert_eq!(manager.get(info.id).expect("get registration"), info);
    let renamed = format!("{name} Renamed");
    manager.rename(info.id, &renamed).expect("rename managed camera");
    assert_eq!(manager.get(info.id).expect("get renamed registration").name, renamed);

    let listed = manager.list().expect("list registrations");
    assert!(listed.iter().any(|camera| camera.id == info.id));
    assert!(listed.iter().any(|camera| camera.id == second.id));

    let mut second_camera = manager.open(second.id).expect("open second independent producer");
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

    let mut camera = manager.open(info.id).expect("open managed producer");
    assert_eq!(camera.camera_type(), CameraType::ManagedNv12);
    assert_eq!(camera.native_format(), PixelFormat::Nv12);
    assert_eq!(camera.frame_size(), 320 * 240 * 3 / 2);

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-f",
            "dshow",
            "-video_size",
            "320x240",
            "-framerate",
            "30",
            "-i",
            &format!("video={renamed}"),
            "-frames:v",
            "3",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg capture");

    let y_size = 320 * 240;
    let mut frame = vec![0_u8; camera.frame_size()];
    frame[..y_size].fill(96);
    frame[y_size..].fill(128);

    let mut exited = false;
    for _ in 0..90 {
        assert_eq!(camera.write_host(&frame).expect("publish NV12 frame"), WriteStatus::Written);
        if child.try_wait().expect("poll ffmpeg").is_some() {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(12));
    }
    camera.close().expect("close producer");

    if !exited {
        child.kill().expect("kill stalled ffmpeg");
    }
    let output = child.wait_with_output().expect("collect ffmpeg output");
    assert!(exited && output.status.success(), "ffmpeg capture failed: {}", String::from_utf8_lossy(&output.stderr));

    manager.unregister(info.id).expect("unregister managed camera");
    manager.unregister(second.id).expect("unregister second managed camera");
    std::mem::forget(guard);
    std::mem::forget(second_guard);
}
