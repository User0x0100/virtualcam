mod obs;
mod protocol;
mod unity_capture;

use crate::{CameraType, OpenOptions, Result, WriteStatus, config::ValidatedConfig};

use self::{obs::ObsBackend, unity_capture::UnityCaptureBackend};

pub(crate) enum Backend {
    Obs(ObsBackend),
    UnityCapture(UnityCaptureBackend),
}

impl Backend {
    pub(crate) fn open(
        camera_type: CameraType,
        config: ValidatedConfig,
        options: OpenOptions,
    ) -> Result<Self> {
        match camera_type {
            CameraType::Obs => ObsBackend::open(config, options).map(Self::Obs),
            CameraType::UnityCapture => UnityCaptureBackend::open(config).map(Self::UnityCapture),
        }
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        match self {
            Self::Obs(backend) => backend.is_ready(),
            Self::UnityCapture(backend) => backend.is_ready(),
        }
    }

    pub(crate) fn write_host_frame(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        match self {
            Self::Obs(backend) => backend.write_host_frame(frame),
            Self::UnityCapture(backend) => backend.write_host_frame(frame),
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        match self {
            Self::Obs(backend) => backend.close(),
            Self::UnityCapture(backend) => backend.close(),
        }
    }
}
