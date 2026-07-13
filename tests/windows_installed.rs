#![cfg(windows)]

use virtualcam::{CameraConfig, CameraType, VirtualCamera, WriteStatus};

#[test]
#[ignore = "requires an installed, idle OBS Virtual Camera"]
fn writes_a_frame_to_installed_obs_virtual_camera() {
    let mut camera = VirtualCamera::open(
        CameraType::Obs,
        CameraConfig {
            width: 2,
            height: 2,
            fps: 30.0,
        },
    )
    .expect("OBS Virtual Camera must be installed and idle");
    assert!(camera.is_ready().expect("query OBS readiness"));
    assert_eq!(
        camera.write_host(&[16, 16, 16, 16, 128, 128]),
        Ok(WriteStatus::Written)
    );
    camera.close().expect("close OBS producer");
}

#[test]
#[ignore = "requires Unity Capture device #1 open in a consumer application"]
fn writes_a_frame_to_installed_unity_capture() {
    let mut camera = VirtualCamera::open(
        CameraType::UnityCapture,
        CameraConfig {
            width: 2,
            height: 2,
            fps: 30.0,
        },
    )
    .expect("Unity Capture must be installed");
    assert!(camera.is_ready().expect("consumer must be active"));
    let rgba = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];
    assert!(matches!(
        camera.write_host(&rgba),
        Ok(WriteStatus::Written | WriteStatus::FrameSkippedByConsumer)
    ));
    camera.close().expect("close Unity Capture producer");
}
