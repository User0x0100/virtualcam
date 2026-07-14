//! A small, protocol-compatible producer for installed Windows virtual cameras.
//!
//! `virtualcam` writes frames to the shared-memory protocols used by OBS Virtual
//! Camera and Unity Capture. OBS accepts [`PixelFormat::Nv12`]; Unity Capture
//! accepts RGBA8 and the upstream gamma/linear RGBA binary16 formats. The crate
//! never scales, converts, schedules, or rate-limits frames.
//!
//! Windows is the only functional platform in this release. The public API is
//! intentionally platform-neutral, so Linux and macOS applications still compile;
//! the backend-specific open methods return [`Error::UnsupportedPlatform`] there.
//!
//! Host frames are published with [`VirtualCamera::write_host`]. With the `cuda`
//! feature, `VirtualCamera::write_cuda` copies device memory into one reusable
//! pinned host buffer, synchronizes only the supplied CUDA stream (or the CUDA
//! default stream for `None`), and then uses the same host publication path. The
//! caller remains responsible for CUDA device, context, stream, and runtime
//! lifetime management.
//!
//! `force_open` is an OBS-only recovery escape hatch. It does not identify or
//! stop another writer. Concurrent writers race on the protocol and produce
//! undefined output.
//!
//! # Features
//!
//! - `cuda`: direct CUDA Runtime FFI and pinned host staging.
//! - `tracing`: emits diagnostic `tracing` events; no subscriber is installed.
//!
//! # Example
//!
//! ```no_run
//! use virtualcam::{ObsOptions, VirtualCamera};
//!
//! # fn run() -> Result<(), virtualcam::Error> {
//! let mut camera = VirtualCamera::open_obs(ObsOptions {
//!     width: 1280,
//!     height: 720,
//!     ..ObsOptions::default()
//! })?;
//! let frame = vec![0_u8; camera.frame_size()];
//! camera.write_host(&frame)?;
//! camera.close()?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_op_in_unsafe_fn)]

macro_rules! trace_debug {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        tracing::debug!($($arg)*);
    }};
}

#[cfg(windows)]
macro_rules! trace_warn {
    ($($arg:tt)*) => {{
        #[cfg(feature = "tracing")]
        tracing::warn!($($arg)*);
    }};
}

mod backend;
mod camera;
mod config;
#[cfg(all(feature = "cuda", windows))]
mod cuda;
#[cfg(all(feature = "cuda", not(windows)))]
#[path = "cuda_unsupported.rs"]
mod cuda;
mod error;
mod pixel_format;
#[cfg(windows)]
mod platform;
mod status;

pub use camera::VirtualCamera;
pub use config::{
    CameraType, ObsFrameRate, ObsOptions, UnityCaptureFormat, UnityCaptureMirrorMode,
    UnityCaptureOptions, UnityCaptureResizeMode,
};
#[cfg(feature = "cuda")]
pub use cuda::CudaStream;
pub use error::{Error, Result};
pub use pixel_format::PixelFormat;
pub use status::WriteStatus;
