use core::ffi::c_void;

#[cfg(windows)]
use cuda_runtime_api::CUDA_MEMCPY_DEVICE_TO_HOST;

#[cfg(windows)]
use crate::Result;

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
    /// stream handle. It must be compatible with the device pointer and current
    /// context used for every call that receives this value. The caller retains
    /// ownership and must not destroy a live stream until the write returns.
    pub const unsafe fn from_raw(raw: *mut c_void) -> Self {
        Self(raw)
    }

    /// Returns the borrowed raw `cudaStream_t`.
    pub const fn as_raw(self) -> *mut c_void {
        self.0
    }
}

/// Copies one contiguous device byte range directly into a backend-owned shared
/// memory frame slot and waits for that stream before the backend publishes it.
#[cfg(windows)]
pub(crate) unsafe fn copy_device_to_shared_memory(host_destination: *mut u8, device_source: *const c_void, byte_len: usize, stream: Option<CudaStream>) -> Result<()> {
    debug_assert!(!host_destination.is_null());
    debug_assert!(!device_source.is_null());
    debug_assert_ne!(byte_len, 0);

    let stream = stream.map_or(core::ptr::null_mut(), CudaStream::as_raw);
    // SAFETY: The backend reserves a writable shared-memory range of `byte_len`
    // bytes until synchronization. The public caller guarantees a compatible,
    // readable CUDA source range and a valid stream/current context.
    unsafe { cuda_runtime_api::cudaMemcpyAsync(host_destination.cast(), device_source, byte_len, CUDA_MEMCPY_DEVICE_TO_HOST, stream) }.check("cudaMemcpyAsync")?;

    // SAFETY: This is the exact stream used by the preceding D2H operation.
    unsafe { cuda_runtime_api::cudaStreamSynchronize(stream) }.check("cudaStreamSynchronize")
}

#[cfg(windows)]
mod cuda_runtime_api {
    use core::ffi::c_void;

    use crate::{Error, Result};

    pub(super) const CUDA_MEMCPY_DEVICE_TO_HOST: i32 = 2;

    /// ABI-compatible representation of CUDA Runtime's `cudaError_t` enum.
    #[allow(non_camel_case_types)]
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct cudaError_t(pub(super) i32);

    impl cudaError_t {
        pub(super) fn check(self, operation: &'static str) -> Result<()> {
            if self.0 == 0 {
                Ok(())
            } else {
                trace_warn!(operation, code = self.0, "CUDA Runtime operation failed");
                Err(Error::Cuda { operation, code: self.0 })
            }
        }
    }

    // CUDA's CUDARTAPI is __stdcall under MSVC. Rust's `system` ABI selects that
    // convention on 32-bit Windows and the platform C ABI on 64-bit Windows.
    //
    // CUDA 13 ships `cudart.lib` as a static loader shim without `__imp_`
    // import thunks, so a default dylib link cannot resolve. Linking it as an
    // unbundled static library references plain symbols, which both that shim
    // and older import-library toolkits provide; the final link still uses the
    // toolkit's own `cudart.lib`.
    #[link(name = "cudart", kind = "static", modifiers = "-bundle")]
    unsafe extern "system" {
        pub(super) fn cudaMemcpyAsync(destination: *mut c_void, source: *const c_void, count: usize, kind: i32, stream: *mut c_void) -> cudaError_t;
        pub(super) fn cudaStreamSynchronize(stream: *mut c_void) -> cudaError_t;
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::cuda_runtime_api::cudaError_t;
    use super::*;
    use crate::Error;

    #[link(name = "cudart", kind = "static", modifiers = "-bundle")]
    unsafe extern "system" {
        fn cudaMalloc(device_pointer: *mut *mut c_void, size: usize) -> cudaError_t;
        fn cudaFree(device_pointer: *mut c_void) -> cudaError_t;
        fn cudaMemcpy(destination: *mut c_void, source: *const c_void, count: usize, kind: i32) -> cudaError_t;
        fn cudaStreamCreateWithFlags(stream: *mut *mut c_void, flags: u32) -> cudaError_t;
        fn cudaStreamDestroy(stream: *mut c_void) -> cudaError_t;
    }

    #[test]
    fn cuda_errors_preserve_operation_and_exact_code() {
        assert_eq!(cudaError_t(17).check("cudaMemcpyAsync"), Err(Error::Cuda { operation: "cudaMemcpyAsync", code: 17 }));
    }

    #[test]
    #[ignore = "requires a CUDA-capable device and Runtime"]
    fn real_cuda_d2h_writes_directly_to_a_windows_shared_mapping() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};

        use crate::platform::windows::{MappedView, OwnedHandle};

        const MAPPING_SIZE: u32 = 4096;
        const DESTINATION_OFFSET: usize = 32;
        const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
        const CUDA_STREAM_NON_BLOCKING: u32 = 1;

        // SAFETY: An unnamed page-file-backed mapping with read/write protection.
        let raw_mapping = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null(), PAGE_READWRITE, 0, MAPPING_SIZE, core::ptr::null()) };
        assert!(!raw_mapping.is_null(), "CreateFileMappingW failed");
        // SAFETY: Successful creation returned one uniquely owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(raw_mapping) };
        let view = MappedView::map(mapping.raw(), FILE_MAP_ALL_ACCESS, "MapViewOfFile(real CUDA test)").expect("map CUDA test shared memory");

        let expected = [0x10_u8, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0];
        let mut device_pointer = core::ptr::null_mut();
        // SAFETY: Writable pointer storage receives an allocation of exact size.
        unsafe { cudaMalloc(&mut device_pointer, expected.len()) }.check("cudaMalloc").expect("allocate real CUDA test source");
        assert!(!device_pointer.is_null());

        // SAFETY: Both buffers contain the requested bytes and kind 1 is H2D.
        unsafe { cudaMemcpy(device_pointer, expected.as_ptr().cast(), expected.len(), CUDA_MEMCPY_HOST_TO_DEVICE) }
            .check("cudaMemcpy")
            .expect("initialize real CUDA test source");

        let mut stream = core::ptr::null_mut();
        // SAFETY: Writable pointer storage receives a new non-blocking stream.
        unsafe { cudaStreamCreateWithFlags(&mut stream, CUDA_STREAM_NON_BLOCKING) }
            .check("cudaStreamCreateWithFlags")
            .expect("create real CUDA test stream");
        assert!(!stream.is_null());

        // SAFETY: The mapped destination contains the entire range; the CUDA
        // allocation and stream remain live until this synchronized call returns.
        unsafe { copy_device_to_shared_memory(view.as_ptr().add(DESTINATION_OFFSET), device_pointer, expected.len(), Some(CudaStream::from_raw(stream))) }.expect("copy CUDA data directly to shared mapping");

        // SAFETY: Synchronization completed and the mapped range remains live.
        assert_eq!(unsafe { core::slice::from_raw_parts(view.as_ptr().add(DESTINATION_OFFSET), expected.len()) }, expected);

        // SAFETY: Both resources were created successfully and are released once.
        unsafe { cudaStreamDestroy(stream) }.check("cudaStreamDestroy").expect("release real CUDA test stream");
        // SAFETY: Pointer came from the successful cudaMalloc above.
        unsafe { cudaFree(device_pointer) }.check("cudaFree").expect("release real CUDA test source");
    }
}
