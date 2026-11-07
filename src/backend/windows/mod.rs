//! Windows producer backends.
//!
//! - `obs`: third-party OBS Virtual Camera queue.
//! - `managed_dshow`: this crate's managed DirectShow camera (producer-owned
//!   runtime format, session-local transport v2).
//! - `managed_mf`: this crate's managed Media Foundation camera (source-owned
//!   native mode, authenticated cross-session transport).
//! - `unity_capture`: third-party Unity Capture shared image.
//!
//! OBS keeps the OBS-compatible three-slot queue; managed cameras use the
//! seqlock'd transport v2 ring from `virtualcam-protocol`.

mod managed_dshow;
mod managed_mf;
mod obs;
mod protocol;
mod queue_writer;
mod unity_capture;

use virtualcam_protocol::ValidatedFormat;

use self::managed_dshow::ManagedDshowBackend;
use self::managed_mf::ManagedMfBackend;
use self::obs::ObsBackend;
use self::protocol::unity_capture::ObjectNames;
use self::unity_capture::UnityCaptureBackend;
use crate::config::{ValidatedObsOptions, ValidatedUnityCaptureOptions};
use crate::platform::windows::registry_key_exists;
use crate::{CameraType, Error, ManagedCameraId, Result, WriteStatus};
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
    ManagedDirectShow(Box<ManagedDshowBackend>),
    ManagedMediaFoundation(Box<ManagedMfBackend>),
    UnityCapture(UnityCaptureBackend),
}

impl Backend {
    pub(crate) fn open_obs(options: ValidatedObsOptions) -> Result<Self> {
        if !registry_key_exists(obs::OBS_CLSID_KEY) {
            return Err(Error::BackendUnavailable(CameraType::Obs));
        }
        ObsBackend::open(options).map(Self::Obs)
    }

    pub(crate) fn open_managed_dshow(id: ManagedCameraId, format: ValidatedFormat) -> Result<Self> {
        ManagedDshowBackend::open(id, format).map(|backend| Self::ManagedDirectShow(Box::new(backend)))
    }

    pub(crate) fn open_managed_mf(id: ManagedCameraId, format: ValidatedFormat, rendezvous_id: &str) -> Result<Self> {
        ManagedMfBackend::open(id, format, rendezvous_id).map(|backend| Self::ManagedMediaFoundation(Box::new(backend)))
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
            Self::ManagedDirectShow(backend) => backend.is_ready(),
            Self::ManagedMediaFoundation(backend) => backend.is_ready(),
            Self::UnityCapture(backend) => backend.is_ready(),
        }
    }

    pub(crate) fn requested_format(&self) -> Option<virtualcam_protocol::VideoFormat> {
        match self {
            Self::ManagedDirectShow(backend) => backend.requested_format(),
            Self::ManagedMediaFoundation(backend) => backend.requested_format(),
            _ => None,
        }
    }

    pub(crate) fn write_host_frame(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        self.write_frame(FrameSource::Host(frame))
    }

    #[cfg(feature = "cuda")]
    pub(crate) unsafe fn write_cuda_frame(&mut self, device_source: *const core::ffi::c_void, byte_len: usize, stream: Option<CudaStream>) -> Result<WriteStatus> {
        self.write_frame(FrameSource::Cuda {
            device_pointer: device_source,
            byte_len,
            stream,
        })
    }

    fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        match self {
            Self::Obs(backend) => backend.write_frame(frame),
            Self::ManagedDirectShow(backend) => backend.write_frame(frame),
            Self::ManagedMediaFoundation(backend) => backend.write_frame(frame),
            Self::UnityCapture(backend) => backend.write_frame(frame),
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        match self {
            Self::Obs(backend) => backend.close(),
            Self::ManagedDirectShow(backend) => backend.close(),
            Self::ManagedMediaFoundation(backend) => backend.close(),
            Self::UnityCapture(backend) => backend.close(),
        }
    }
}
