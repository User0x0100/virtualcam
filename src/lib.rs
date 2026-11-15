//! Windows virtual-camera producers and managed DirectShow / Media Foundation sources.
//!
//! [`ManagedCameraManager`] registers and manages native Rust DirectShow and
//! Media Foundation cameras with configurable video formats. The library also
//! publishes frames to the installed OBS Virtual Camera (NV12) and Unity
//! Capture (RGBA8 or gamma/linear FP16) shared-memory protocols. Producers
//! do not resize or convert frames; Media Foundation sources can convert
//! NV12/I420 to YUY2 for consumers that select that format.
//!
//! Windows is the only functional platform in this release. The public API is
//! intentionally platform-neutral, so Linux and macOS applications still compile;
//! the backend-specific open methods return [`Error::UnsupportedPlatform`] there.
//!
//! Host frames are published with [`VirtualCamera::write_host`]. With the `cuda`
//! feature, `VirtualCamera::write_cuda` copies one contiguous device byte range
//! directly into the backend's Windows shared-memory frame slot, synchronizes the
//! supplied CUDA stream (or the null stream for `None`), and only then publishes
//! it. The crate allocates no intermediate buffer; the caller remains responsible
//! for CUDA device, context, stream, and runtime lifetime management.
//!
//! `force_open` is an OBS-only recovery escape hatch. It does not identify or
//! stop another writer. Concurrent writers race on the protocol and produce
//! undefined output.
//!
//! # Features
//!
//! - `cuda`: direct CUDA Runtime D2H FFI with no crate-owned staging allocation.
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
#[cfg(feature = "cuda")]
mod cuda;
mod error;
mod managed;
#[cfg(windows)]
mod platform;
mod status;

pub use camera::VirtualCamera;
pub use config::{CameraType, ObsOptions, UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions, UnityCaptureResizeMode};
#[cfg(feature = "cuda")]
pub use cuda::CudaStream;
pub use error::{Error, Result};
pub use managed::{AppId, DirectShowRegistration, ManagedBackend, ManagedCameraId, ManagedCameraInfo, ManagedCameraManager, ManagedCameraRegistration, MediaFoundationRegistration, RegistrationHealth, RegistrationScope};
pub use status::WriteStatus;
pub use virtualcam_protocol::{ColorMatrix, ColorPrimaries, ColorRange, ColorSpace, FormatError, FrameRate, PixelFormat, TransferFunction, VideoFormat};
