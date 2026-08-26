use core::fmt;

use crate::CameraType;

/// Result alias used by this crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors returned while validating, opening, transferring, or publishing frames.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Virtual-camera backends are not implemented on the current operating system.
    UnsupportedPlatform,
    /// The selected virtual-camera driver is not installed for this process architecture.
    BackendUnavailable(CameraType),
    /// Dimensions/stride are zero, invalid for the format, exceed Unity's
    /// shared-image capacity, or are not representable by the backend ABI.
    InvalidDimensions,
    /// An OBS/managed-camera frame-rate numerator/denominator is zero or produces a zero interval.
    InvalidFrameRate,
    /// A managed camera friendly name is empty, too long, or contains an embedded NUL.
    InvalidCameraName,
    /// A managed camera CLSID string is malformed.
    InvalidCameraId,
    /// A frame or shared-memory layout size overflowed its target integer type.
    FrameSizeOverflow,
    /// A host or CUDA frame length did not match the configured contiguous layout.
    FrameSizeMismatch {
        /// Required byte length.
        expected: usize,
        /// Supplied byte length.
        actual: usize,
    },
    /// The conservative OBS producer occupancy check found an existing mapping.
    DeviceInUse,
    /// A force-opened OBS mapping cannot hold the configured queue layout.
    ExistingMappingTooSmall {
        /// Required accessible byte length.
        required: usize,
        /// Accessible byte length reported for the mapped view.
        available: usize,
    },
    /// A backend operation requires a consumer that has not become active.
    ConsumerInactive,
    /// An operation other than idempotent `close` was attempted after closure.
    AlreadyClosed,
    /// The loaded native managed-camera DLL is missing an expected ABI export.
    ManagedDriverAbi {
        /// Missing symbol or incompatible ABI field.
        symbol: &'static str,
    },
    /// A native COM/DirectShow operation returned a failing HRESULT.
    Hresult {
        /// Native operation that failed.
        operation: &'static str,
        /// Exact signed HRESULT value.
        code: i32,
    },
    /// A Win32 call failed and preserved its `GetLastError` code.
    Windows {
        /// Win32 API or logical operation that failed.
        operation: &'static str,
        /// Exact `GetLastError` value.
        code: u32,
    },
    /// A CUDA Runtime call failed.
    Cuda {
        /// CUDA Runtime API that failed.
        operation: &'static str,
        /// Exact `cudaError_t` value.
        code: i32,
    },
    /// Shared protocol state was incompatible with a safe publication.
    Protocol {
        /// Backend whose protocol state was invalid.
        backend: CameraType,
        /// Static description of the violated invariant.
        detail: &'static str,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => formatter.write_str("virtual cameras are unsupported on this platform"),
            Self::BackendUnavailable(backend) => {
                write!(formatter, "{backend:?} virtual camera is not installed")
            }
            Self::InvalidDimensions => formatter.write_str("invalid frame dimensions"),
            Self::InvalidFrameRate => formatter.write_str("invalid frame rate"),
            Self::InvalidCameraName => formatter.write_str("invalid managed camera name"),
            Self::InvalidCameraId => formatter.write_str("invalid managed camera CLSID"),
            Self::FrameSizeOverflow => formatter.write_str("frame or shared-memory size overflow"),
            Self::FrameSizeMismatch { expected, actual } => {
                write!(formatter, "frame size mismatch: expected {expected} bytes, got {actual}")
            }
            Self::DeviceInUse => formatter.write_str("virtual camera producer is already in use"),
            Self::ExistingMappingTooSmall { required, available } => write!(formatter, "existing mapping is too small: requires {required} accessible bytes, has {available}"),
            Self::ConsumerInactive => formatter.write_str("virtual camera consumer is inactive"),
            Self::AlreadyClosed => formatter.write_str("virtual camera is already closed"),
            Self::ManagedDriverAbi { symbol } => write!(formatter, "managed virtual-camera driver ABI is missing or incompatible at {symbol}"),
            Self::Hresult { operation, code } => write!(formatter, "native operation {operation} failed with HRESULT 0x{:08X}", *code as u32),
            Self::Windows { operation, code } => {
                write!(formatter, "Win32 operation {operation} failed with code {code}")
            }
            Self::Cuda { operation, code } => {
                write!(formatter, "CUDA operation {operation} failed with code {code}")
            }
            Self::Protocol { backend, detail } => {
                write!(formatter, "{backend:?} protocol error: {detail}")
            }
        }
    }
}

impl std::error::Error for Error {}
