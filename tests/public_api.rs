#[cfg(windows)]
use virtualcam::CameraType;
use virtualcam::{Error, ObsFrameRate, ObsOptions, PixelFormat, UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions, UnityCaptureResizeMode, VirtualCamera};

#[test]
fn pixel_format_validates_dimensions_and_checked_sizes() {
    assert_eq!(PixelFormat::Nv12.frame_size(1920, 1080), Ok(3_110_400));
    assert_eq!(PixelFormat::Rgba8.frame_size(1920, 1080), Ok(8_294_400));

    for format in [PixelFormat::Nv12, PixelFormat::Rgba8, PixelFormat::Rgba16FloatGamma, PixelFormat::Rgba16FloatLinear] {
        assert_eq!(format.frame_size(0, 2), Err(Error::InvalidDimensions));
        assert_eq!(format.frame_size(2, 0), Err(Error::InvalidDimensions));
    }
    assert_eq!(PixelFormat::Nv12.frame_size(3, 2), Err(Error::InvalidDimensions));
    assert_eq!(PixelFormat::Nv12.frame_size(2, 3), Err(Error::InvalidDimensions));
    assert_eq!(PixelFormat::Rgba8.frame_size(u32::MAX, u32::MAX), Err(Error::FrameSizeOverflow));
    assert_eq!(PixelFormat::Rgba16FloatLinear.frame_size(2, 3), Ok(48));
}

#[test]
fn backend_options_are_independent_and_match_upstream_defaults() {
    assert_eq!(ObsOptions::default().frame_rate, ObsFrameRate { numerator: 30, denominator: 1 });
    assert!(!ObsOptions::default().force_open);

    let unity = UnityCaptureOptions::default();
    assert_eq!(unity.capture_number, 0);
    assert_eq!(unity.format, UnityCaptureFormat::Rgba8);
    assert_eq!(unity.format.bytes_per_pixel(), 4);
    assert_eq!(unity.stride_pixels, None);
    assert_eq!(unity.resize_mode, UnityCaptureResizeMode::Disabled);
    assert_eq!(unity.mirror_mode, UnityCaptureMirrorMode::Disabled);
    assert_eq!(unity.timeout_ms, 1000);
}

#[test]
fn cameras_can_move_to_a_dedicated_producer_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<VirtualCamera>();
    assert_send::<virtualcam::ManagedCameraManager>();
}

#[test]
fn errors_preserve_useful_display_context() {
    assert_eq!(Error::FrameSizeMismatch { expected: 12, actual: 9 }.to_string(), "frame size mismatch: expected 12 bytes, got 9");
    assert_eq!(Error::Windows { operation: "MapViewOfFile", code: 5 }.to_string(), "Win32 operation MapViewOfFile failed with code 5");
}

#[cfg(windows)]
#[test]
fn open_rejects_invalid_configuration_before_driver_discovery() {
    let zero_width = ObsOptions {
        width: 0,
        height: 2,
        ..ObsOptions::default()
    };
    assert!(matches!(VirtualCamera::open_obs(zero_width), Err(Error::InvalidDimensions)));

    let invalid_rate = ObsOptions {
        width: 2,
        height: 2,
        frame_rate: ObsFrameRate { numerator: 0, denominator: 1 },
        force_open: false,
    };
    assert!(matches!(VirtualCamera::open_obs(invalid_rate), Err(Error::InvalidFrameRate)));

    assert!(matches!(
        VirtualCamera::open_unity_capture(UnityCaptureOptions {
            capture_number: UnityCaptureOptions::MAX_CAPTURE_NUMBER + 1,
            ..UnityCaptureOptions::default()
        }),
        Err(Error::Protocol { backend: CameraType::UnityCapture, .. })
    ));
}

#[cfg(not(windows))]
#[test]
fn open_reports_unsupported_platform() {
    assert!(matches!(VirtualCamera::open_obs(ObsOptions::default()), Err(Error::UnsupportedPlatform)));
    assert!(matches!(VirtualCamera::open_unity_capture(UnityCaptureOptions::default()), Err(Error::UnsupportedPlatform)));
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
