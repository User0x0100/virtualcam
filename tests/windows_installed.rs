#![cfg(windows)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::{env, fs};

use virtualcam::{CameraType, Error, ObsFrameRate, ObsOptions, PixelFormat, UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions, UnityCaptureResizeMode, VirtualCamera, WriteStatus};

// Both installed-driver tests touch globally named protocol objects. Serializing
// them also makes failures easier to attribute when they are explicitly enabled.
static INSTALLED_DRIVER_LOCK: Mutex<()> = Mutex::new(());

/// Restores `%APPDATA%\obs-virtualcam.txt`, which every OBS producer open
/// rewrites so the OBS filter can advertise the last producer mode.
struct ObsResolutionHintGuard {
    path: PathBuf,
    original: Option<Vec<u8>>,
}

impl ObsResolutionHintGuard {
    fn capture() -> Self {
        let path = PathBuf::from(env::var_os("APPDATA").expect("APPDATA is set for interactive users")).join("obs-virtualcam.txt");
        let original = fs::read(&path).ok();
        Self { path, original }
    }

    fn current(&self) -> Option<Vec<u8>> {
        fs::read(&self.path).ok()
    }
}

impl Drop for ObsResolutionHintGuard {
    fn drop(&mut self) {
        let _ = match &self.original {
            Some(contents) => fs::write(&self.path, contents),
            None => fs::remove_file(&self.path),
        };
    }
}

fn tiny_obs_options() -> ObsOptions {
    ObsOptions {
        width: 2,
        height: 2,
        frame_rate: ObsFrameRate::default(),
        force_open: false,
    }
}

fn tiny_unity_options() -> UnityCaptureOptions {
    UnityCaptureOptions {
        width: 2,
        height: 2,
        ..UnityCaptureOptions::default()
    }
}

#[test]
#[ignore = "requires an installed, idle OBS Virtual Camera"]
fn installed_obs_covers_metadata_publication_occupancy_and_lifecycle() {
    let _serial = INSTALLED_DRIVER_LOCK.lock().expect("installed-driver test lock");
    let hint = ObsResolutionHintGuard::capture();
    let mut camera = VirtualCamera::open_obs(tiny_obs_options()).expect("OBS Virtual Camera must be installed and idle");
    // Same `{cx}x{cy}x{interval}` record OBS writes in `virtualcam_start`.
    assert_eq!(hint.current().as_deref(), Some(&b"2x2x333333"[..]));

    assert_eq!(camera.camera_type(), CameraType::Obs);
    assert_eq!(camera.native_format(), PixelFormat::Nv12);
    assert_eq!(camera.frame_size(), 6);
    assert!(camera.is_open());
    assert_eq!(camera.is_ready(), Ok(true));
    assert_eq!(camera.write_host(&[0; 5]), Err(Error::FrameSizeMismatch { expected: 6, actual: 5 }));

    // A rejected second producer must not overwrite the owner's hint.
    assert!(matches!(VirtualCamera::open_obs(ObsOptions { width: 4, ..tiny_obs_options() }), Err(Error::DeviceInUse)));
    assert_eq!(hint.current().as_deref(), Some(&b"2x2x333333"[..]));
    for frame in [[16, 16, 16, 16, 128, 128], [32, 32, 32, 32, 96, 160], [235, 235, 235, 235, 128, 128]] {
        assert_eq!(camera.write_host(&frame), Ok(WriteStatus::Written));
    }

    #[cfg(feature = "cuda")]
    {
        assert!(matches!(
            // SAFETY: Null is intentionally rejected before any CUDA call.
            unsafe { camera.write_cuda(std::ptr::null(), camera.frame_size(), None) },
            Err(Error::Protocol {
                backend: CameraType::Obs,
                detail: "CUDA device pointer is null",
            })
        ));
        assert_eq!(
            // SAFETY: Length mismatch is intentionally rejected before pointer use.
            unsafe { camera.write_cuda(std::ptr::null(), camera.frame_size() - 1, None) },
            Err(Error::FrameSizeMismatch { expected: 6, actual: 5 })
        );
    }

    camera.close().expect("close OBS producer");
    assert!(!camera.is_open());
    assert_eq!(camera.close(), Ok(()));
    assert_eq!(camera.is_ready(), Err(Error::AlreadyClosed));
    assert_eq!(camera.write_host(&[0; 6]), Err(Error::AlreadyClosed));

    // Normal reopen proves the previous mapping handle and view were released.
    let reopened = VirtualCamera::open_obs(tiny_obs_options()).expect("OBS mapping must be reusable after close");
    drop(reopened);

    // With no competing writer, force mode remains a valid explicit open path.
    let mut forced = VirtualCamera::open_obs(ObsOptions { force_open: true, ..tiny_obs_options() }).expect("force-open OBS while idle");
    assert_eq!(forced.write_host(&[16, 16, 16, 16, 128, 128]), Ok(WriteStatus::Written));
    forced.close().expect("close force-open OBS producer");
}

#[test]
#[ignore = "requires an installed Unity Capture virtual camera"]
fn installed_unity_covers_inactive_or_active_consumer_and_lifecycle() {
    let _serial = INSTALLED_DRIVER_LOCK.lock().expect("installed-driver test lock");
    let mut camera = VirtualCamera::open_unity_capture(tiny_unity_options()).expect("Unity Capture must be installed for this process architecture");

    assert_eq!(camera.camera_type(), CameraType::UnityCapture);
    assert_eq!(camera.native_format(), PixelFormat::Rgba8);
    assert_eq!(camera.frame_size(), 16);
    assert!(camera.is_open());
    assert_eq!(camera.write_host(&[0; 15]), Err(Error::FrameSizeMismatch { expected: 16, actual: 15 }));

    let ready_before_write = camera.is_ready().expect("query Unity readiness");
    let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
    let status = camera.write_host(&rgba).expect("publish or report inactivity");
    assert!(matches!(status, WriteStatus::Written | WriteStatus::FrameSkippedByConsumer | WriteStatus::ConsumerInactive));
    if ready_before_write {
        assert_ne!(status, WriteStatus::ConsumerInactive);
    }

    #[cfg(feature = "cuda")]
    assert!(matches!(
        // SAFETY: Null is intentionally rejected before any CUDA call.
        unsafe { camera.write_cuda(std::ptr::null(), camera.frame_size(), None) },
        Err(Error::Protocol {
            backend: CameraType::UnityCapture,
            detail: "CUDA device pointer is null",
        })
    ));

    camera.close().expect("close Unity producer");
    assert!(!camera.is_open());
    assert_eq!(camera.close(), Ok(()));
    assert_eq!(camera.is_ready(), Err(Error::AlreadyClosed));
    assert_eq!(camera.write_host(&rgba), Err(Error::AlreadyClosed));

    let mut controlled = VirtualCamera::open_unity_capture(UnityCaptureOptions {
        format: UnityCaptureFormat::Rgba16FloatLinear,
        resize_mode: UnityCaptureResizeMode::Linear,
        mirror_mode: UnityCaptureMirrorMode::Horizontal,
        timeout_ms: 250,
        ..tiny_unity_options()
    })
    .expect("Unity producer reopens with non-default controls");
    assert_eq!(controlled.native_format(), PixelFormat::Rgba16FloatLinear);
    assert_eq!(controlled.frame_size(), 32);
    assert!(matches!(controlled.write_host(&[0; 32]), Ok(WriteStatus::Written | WriteStatus::FrameSkippedByConsumer | WriteStatus::ConsumerInactive)));
    controlled.close().expect("close controlled Unity producer");
}
