#![cfg(windows)]

use std::sync::Mutex;

use virtualcam::{
    CameraConfig, CameraType, Error, OpenOptions, PixelFormat, VirtualCamera, WriteStatus,
};

// Both installed-driver tests touch globally named protocol objects. Serializing
// them also makes failures easier to attribute when they are explicitly enabled.
static INSTALLED_DRIVER_LOCK: Mutex<()> = Mutex::new(());

fn tiny_config() -> CameraConfig {
    CameraConfig {
        width: 2,
        height: 2,
        fps: 30.0,
    }
}

#[test]
#[ignore = "requires an installed, idle OBS Virtual Camera"]
fn installed_obs_covers_metadata_publication_occupancy_and_lifecycle() {
    let _serial = INSTALLED_DRIVER_LOCK
        .lock()
        .expect("installed-driver test lock");
    let mut camera = VirtualCamera::open(CameraType::Obs, tiny_config())
        .expect("OBS Virtual Camera must be installed and idle");

    assert_eq!(camera.camera_type(), CameraType::Obs);
    assert_eq!(camera.native_format(), PixelFormat::Nv12);
    assert_eq!(camera.frame_size(), 6);
    assert!(camera.is_open());
    assert_eq!(camera.is_ready(), Ok(true));
    assert_eq!(
        camera.write_host(&[0; 5]),
        Err(Error::FrameSizeMismatch {
            expected: 6,
            actual: 5,
        })
    );

    assert!(matches!(
        VirtualCamera::open(CameraType::Obs, tiny_config()),
        Err(Error::DeviceInUse)
    ));
    for frame in [
        [16, 16, 16, 16, 128, 128],
        [32, 32, 32, 32, 96, 160],
        [235, 235, 235, 235, 128, 128],
    ] {
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
            Err(Error::FrameSizeMismatch {
                expected: 6,
                actual: 5,
            })
        );
    }

    camera.close().expect("close OBS producer");
    assert!(!camera.is_open());
    assert_eq!(camera.close(), Ok(()));
    assert_eq!(camera.is_ready(), Err(Error::AlreadyClosed));
    assert_eq!(camera.write_host(&[0; 6]), Err(Error::AlreadyClosed));

    // Normal reopen proves the previous mapping handle and view were released.
    let reopened = VirtualCamera::open(CameraType::Obs, tiny_config())
        .expect("OBS mapping must be reusable after close");
    drop(reopened);

    // With no competing writer, force mode remains a valid explicit open path.
    let mut forced = VirtualCamera::open_with_options(
        CameraType::Obs,
        tiny_config(),
        OpenOptions { force_open: true },
    )
    .expect("force-open OBS while idle");
    assert_eq!(
        forced.write_host(&[16, 16, 16, 16, 128, 128]),
        Ok(WriteStatus::Written)
    );
    forced.close().expect("close force-open OBS producer");
}

#[test]
#[ignore = "requires an installed Unity Capture virtual camera"]
fn installed_unity_covers_inactive_or_active_consumer_and_lifecycle() {
    let _serial = INSTALLED_DRIVER_LOCK
        .lock()
        .expect("installed-driver test lock");
    let mut camera = VirtualCamera::open(CameraType::UnityCapture, tiny_config())
        .expect("Unity Capture must be installed for this process architecture");

    assert_eq!(camera.camera_type(), CameraType::UnityCapture);
    assert_eq!(camera.native_format(), PixelFormat::Rgba8);
    assert_eq!(camera.frame_size(), 16);
    assert!(camera.is_open());
    assert_eq!(
        camera.write_host(&[0; 15]),
        Err(Error::FrameSizeMismatch {
            expected: 16,
            actual: 15,
        })
    );

    let ready_before_write = camera.is_ready().expect("query Unity readiness");
    let rgba = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    let status = camera
        .write_host(&rgba)
        .expect("publish or report inactivity");
    assert!(matches!(
        status,
        WriteStatus::Written | WriteStatus::FrameSkippedByConsumer | WriteStatus::ConsumerInactive
    ));
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

    // force_open is documented as an OBS-only switch and must not prevent Unity.
    let reopened = VirtualCamera::open_with_options(
        CameraType::UnityCapture,
        tiny_config(),
        OpenOptions { force_open: true },
    )
    .expect("Unity ignores OBS force-open option");
    drop(reopened);
}
