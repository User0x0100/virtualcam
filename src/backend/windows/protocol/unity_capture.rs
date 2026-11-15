use core::mem::{align_of, offset_of, size_of};
use std::ffi::CString;

use crate::{CameraType, Error, Result, UnityCaptureOptions};

pub(crate) const MAX_CAPTURE_NUMBER: u8 = UnityCaptureOptions::MAX_CAPTURE_NUMBER;
pub(crate) const MAX_SHARED_IMAGE_SIZE: usize = UnityCaptureOptions::MAX_FRAME_BYTES;

/// Mirrors `SharedImageMemory::SharedMemHeader` from
/// schellingb/UnityCapture@3ed54c325e0ad71afcf4f246c07e5e17b3d7f2d2,
/// `Source/shared.inl`.
///
/// `DWORD` is `u32`; every C++ `int` is `i32`. The one-byte trailing array makes
/// `sizeof(SharedMemHeader)` 36 while its image data starts at byte 32.
#[repr(C)]
pub(crate) struct SharedMemHeader {
    pub(crate) max_size: u32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) stride: i32,
    pub(crate) format: i32,
    pub(crate) resize_mode: i32,
    pub(crate) mirror_mode: i32,
    pub(crate) timeout: i32,
    pub(crate) data: [u8; 1],
}

pub(crate) const DATA_OFFSET: usize = offset_of!(SharedMemHeader, data);

pub(crate) struct ObjectNames {
    pub(crate) mutex: CString,
    pub(crate) want: CString,
    pub(crate) sent: CString,
    pub(crate) data: CString,
}

impl ObjectNames {
    pub(crate) fn for_capture(capture_number: u8) -> Result<Self> {
        Ok(Self {
            mutex: object_name("UnityCapture_Mutx", capture_number)?,
            want: object_name("UnityCapture_Want", capture_number)?,
            sent: object_name("UnityCapture_Sent", capture_number)?,
            data: object_name("UnityCapture_Data", capture_number)?,
        })
    }
}

fn object_name(base: &str, capture_number: u8) -> Result<CString> {
    if capture_number > MAX_CAPTURE_NUMBER {
        return Err(Error::Protocol {
            backend: CameraType::UnityCapture,
            detail: "capture number exceeds Unity Capture protocol maximum",
        });
    }
    let mut bytes = base.as_bytes().to_vec();
    if capture_number != 0 {
        bytes.push(b'0' + capture_number);
    }
    CString::new(bytes).map_err(|_| Error::Protocol {
        backend: CameraType::UnityCapture,
        detail: "Unity Capture object name contains an interior NUL",
    })
}

const _: () = {
    assert!(size_of::<SharedMemHeader>() == 36);
    assert!(align_of::<SharedMemHeader>() == 4);
    assert!(offset_of!(SharedMemHeader, max_size) == 0);
    assert!(offset_of!(SharedMemHeader, width) == 4);
    assert!(offset_of!(SharedMemHeader, height) == 8);
    assert!(offset_of!(SharedMemHeader, stride) == 12);
    assert!(offset_of!(SharedMemHeader, format) == 16);
    assert!(offset_of!(SharedMemHeader, resize_mode) == 20);
    assert!(offset_of!(SharedMemHeader, mirror_mode) == 24);
    assert!(offset_of!(SharedMemHeader, timeout) == 28);
    assert!(DATA_OFFSET == 32);
    assert!(size_of::<i32>() == 4);
    assert!(size_of::<u32>() == 4);
    assert!(size_of::<u64>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureResizeMode};

    #[test]
    fn shared_header_matches_msvc_c_layout() {
        assert_eq!(size_of::<SharedMemHeader>(), 36);
        assert_eq!(align_of::<SharedMemHeader>(), 4);
        assert_eq!(DATA_OFFSET, 32);
    }

    #[test]
    fn capture_zero_uses_unsuffixed_protocol_names() {
        let names = ObjectNames::for_capture(0).expect("valid capture number");
        assert_eq!(names.mutex.to_bytes(), b"UnityCapture_Mutx");
        assert_eq!(names.want.to_bytes(), b"UnityCapture_Want");
        assert_eq!(names.sent.to_bytes(), b"UnityCapture_Sent");
        assert_eq!(names.data.to_bytes(), b"UnityCapture_Data");
    }

    #[test]
    fn capture_suffix_uses_ascii_offset_rule() {
        let names = ObjectNames::for_capture(1).expect("valid capture number");
        assert_eq!(names.mutex.to_bytes(), b"UnityCapture_Mutx1");
        let names = ObjectNames::for_capture(MAX_CAPTURE_NUMBER).expect("maximum capture number");
        assert_eq!(names.mutex.to_bytes(), b"UnityCapture_Mutxz");
    }

    #[test]
    fn rejects_capture_number_above_protocol_maximum() {
        assert!(matches!(
            ObjectNames::for_capture(MAX_CAPTURE_NUMBER + 1),
            Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "capture number exceeds Unity Capture protocol maximum",
            })
        ));
    }

    #[test]
    fn protocol_constants_match_unity_capture_contract() {
        assert_eq!(MAX_CAPTURE_NUMBER, 74);
        assert_eq!(MAX_SHARED_IMAGE_SIZE, 66_355_200);
        assert_eq!(UnityCaptureFormat::Rgba8 as i32, 0);
        assert_eq!(UnityCaptureFormat::Rgba16FloatGamma as i32, 1);
        assert_eq!(UnityCaptureFormat::Rgba16FloatLinear as i32, 2);
        assert_eq!(UnityCaptureResizeMode::Disabled as i32, 0);
        assert_eq!(UnityCaptureResizeMode::Linear as i32, 1);
        assert_eq!(UnityCaptureMirrorMode::Disabled as i32, 0);
        assert_eq!(UnityCaptureMirrorMode::Horizontal as i32, 1);
        assert_eq!(UnityCaptureOptions::default().timeout_ms, 1000);
    }
}
