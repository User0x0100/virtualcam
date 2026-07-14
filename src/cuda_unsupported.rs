use core::ffi::c_void;

/// An opaque CUDA Runtime stream handle borrowed from the caller.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CudaStream(*mut c_void);

impl CudaStream {
    /// Wraps a raw `cudaStream_t` without taking ownership.
    ///
    /// # Safety
    ///
    /// `raw` must be null, a live CUDA Runtime stream, or a CUDA-defined special
    /// stream handle compatible with the pointer and current context used by the
    /// caller. The caller retains ownership. Non-Windows virtual-camera
    /// operations still return `UnsupportedPlatform`.
    pub const unsafe fn from_raw(raw: *mut c_void) -> Self {
        Self(raw)
    }

    /// Returns the borrowed raw `cudaStream_t`.
    pub const fn as_raw(self) -> *mut c_void {
        self.0
    }
}
