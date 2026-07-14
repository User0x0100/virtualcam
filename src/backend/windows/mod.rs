mod obs;
mod protocol;
mod unity_capture;

use crate::{
    Result, WriteStatus,
    config::{ValidatedObsOptions, ValidatedUnityCaptureOptions},
};

use self::{obs::ObsBackend, unity_capture::UnityCaptureBackend};

pub(crate) enum Backend {
    Obs(ObsBackend),
    UnityCapture(UnityCaptureBackend),
}

impl Backend {
    pub(crate) fn open_obs(options: ValidatedObsOptions) -> Result<Self> {
        ObsBackend::open(options).map(Self::Obs)
    }

    pub(crate) fn open_unity_capture(options: ValidatedUnityCaptureOptions) -> Result<Self> {
        UnityCaptureBackend::open(options).map(Self::UnityCapture)
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
