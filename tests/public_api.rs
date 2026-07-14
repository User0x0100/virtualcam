use virtualcam::{CameraConfig, CameraType, Error, OpenOptions, PixelFormat, VirtualCamera};

#[test]
fn pixel_format_validates_dimensions_and_checked_sizes() {
    assert_eq!(PixelFormat::Nv12.frame_size(1920, 1080), Ok(3_110_400));
    assert_eq!(PixelFormat::Rgba8.frame_size(1920, 1080), Ok(8_294_400));

    for format in [PixelFormat::Nv12, PixelFormat::Rgba8] {
        assert_eq!(format.frame_size(0, 2), Err(Error::InvalidDimensions));
        assert_eq!(format.frame_size(2, 0), Err(Error::InvalidDimensions));
    }
    assert_eq!(
        PixelFormat::Nv12.frame_size(3, 2),
        Err(Error::InvalidDimensions)
    );
    assert_eq!(
        PixelFormat::Nv12.frame_size(2, 3),
        Err(Error::InvalidDimensions)
    );
    assert_eq!(
        PixelFormat::Rgba8.frame_size(u32::MAX, u32::MAX),
        Err(Error::FrameSizeOverflow)
    );
}

#[test]
fn open_options_do_not_force_open_by_default() {
    assert!(!OpenOptions::default().force_open);
}

#[test]
fn errors_preserve_useful_display_context() {
    assert_eq!(
        Error::FrameSizeMismatch {
            expected: 12,
            actual: 9,
        }
        .to_string(),
        "frame size mismatch: expected 12 bytes, got 9"
    );
    assert_eq!(
        Error::Windows {
            operation: "MapViewOfFile",
            code: 5,
        }
        .to_string(),
        "Win32 operation MapViewOfFile failed with code 5"
    );
}

#[cfg(windows)]
#[test]
fn open_rejects_invalid_configuration_before_driver_discovery() {
    let zero_width = CameraConfig {
        width: 0,
        height: 2,
        fps: 30.0,
    };
    assert!(matches!(
        VirtualCamera::open(CameraType::Obs, zero_width),
        Err(Error::InvalidDimensions)
    ));

    let invalid_rate = CameraConfig {
        width: 2,
        height: 2,
        fps: f64::NAN,
    };
    assert!(matches!(
        VirtualCamera::open(CameraType::UnityCapture, invalid_rate),
        Err(Error::InvalidFrameRate)
    ));
}

#[cfg(not(windows))]
#[test]
fn open_reports_unsupported_platform() {
    let config = CameraConfig {
        width: 2,
        height: 2,
        fps: 30.0,
    };
    assert!(matches!(
        VirtualCamera::open(CameraType::Obs, config),
        Err(Error::UnsupportedPlatform)
    ));
}

#[cfg(feature = "cuda")]
#[test]
fn cuda_stream_round_trips_opaque_raw_handle() {
    use core::ffi::c_void;
    use virtualcam::CudaStream;

    let raw = 0x1234_usize as *mut c_void;
    // SAFETY: The opaque sentinel is never submitted to CUDA in this API test.
    let stream = unsafe { CudaStream::from_raw(raw) };
    assert_eq!(stream.as_raw(), raw);
}
