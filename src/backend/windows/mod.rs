mod obs;
mod protocol;
mod unity_capture;

use self::obs::ObsBackend;
use self::protocol::obs::VIDEO_NAME;
use self::protocol::unity_capture::ObjectNames;
use self::unity_capture::UnityCaptureBackend;
use crate::config::{ValidatedObsOptions, ValidatedUnityCaptureOptions};
use crate::platform::windows::registry_key_exists;
use crate::{CameraType, Error, Result, WriteStatus};
#[cfg(feature = "cuda")]
use crate::{CudaStream, cuda};

pub(super) enum FrameSource<'a> {
    Host(&'a [u8]),
    #[cfg(feature = "cuda")]
    Cuda {
        device_pointer: *const core::ffi::c_void,
        byte_len: usize,
        stream: Option<CudaStream>,
    },
}

impl FrameSource<'_> {
    fn byte_len(&self) -> usize {
        match self {
            Self::Host(frame) => frame.len(),
            #[cfg(feature = "cuda")]
            Self::Cuda { byte_len, .. } => *byte_len,
        }
    }

    fn copy_to(self, destination: *mut u8) -> Result<()> {
        match self {
            Self::Host(frame) => {
                // SAFETY: The backend provides a distinct writable slot whose
                // size was checked against this contiguous slice.
                unsafe {
                    core::ptr::copy_nonoverlapping(frame.as_ptr(), destination, frame.len());
                }
                Ok(())
            }
            #[cfg(feature = "cuda")]
            Self::Cuda { device_pointer, byte_len, stream } => {
                // SAFETY: Constructed only by write_cuda_frame after the public
                // caller accepted the device-pointer and stream contract.
                unsafe { cuda::copy_device_to_shared_memory(destination, device_pointer, byte_len, stream) }
            }
        }
    }
}

pub(crate) enum Backend {
    Obs(ObsBackend),
    UnityCapture(UnityCaptureBackend),
}

impl Backend {
    pub(crate) fn open_obs(options: ValidatedObsOptions) -> Result<Self> {
        if !registry_key_exists(obs::OBS_CLSID_KEY) {
            return Err(Error::BackendUnavailable(CameraType::Obs));
        }
        ObsBackend::open(options, VIDEO_NAME).map(Self::Obs)
    }

    pub(crate) fn open_unity_capture(options: ValidatedUnityCaptureOptions) -> Result<Self> {
        if !registry_key_exists(&unity_capture::unity_clsid_key(options.capture_number)) {
            return Err(Error::BackendUnavailable(CameraType::UnityCapture));
        }
        UnityCaptureBackend::open(options, ObjectNames::for_capture(options.capture_number)?).map(Self::UnityCapture)
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        match self {
            Self::Obs(backend) => backend.is_ready(),
            Self::UnityCapture(backend) => backend.is_ready(),
        }
    }

    pub(crate) fn write_host_frame(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        match self {
            Self::Obs(backend) => backend.write_frame(FrameSource::Host(frame)),
            Self::UnityCapture(backend) => backend.write_frame(FrameSource::Host(frame)),
        }
    }

    #[cfg(feature = "cuda")]
    pub(crate) unsafe fn write_cuda_frame(&mut self, device_source: *const core::ffi::c_void, byte_len: usize, stream: Option<CudaStream>) -> Result<WriteStatus> {
        let frame = FrameSource::Cuda {
            device_pointer: device_source,
            byte_len,
            stream,
        };
        match self {
            Self::Obs(backend) => backend.write_frame(frame),
            Self::UnityCapture(backend) => backend.write_frame(frame),
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        match self {
            Self::Obs(backend) => backend.close(),
            Self::UnityCapture(backend) => backend.close(),
        }
    }
}
