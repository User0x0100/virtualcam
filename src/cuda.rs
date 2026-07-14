use core::ffi::c_void;

use crate::Result;

use cuda_runtime_api::{CUDA_MEMCPY_DEVICE_TO_HOST, cudaError_t};

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
pub(crate) unsafe fn copy_device_to_shared_memory(
    host_destination: *mut u8,
    device_source: *const c_void,
    byte_len: usize,
    stream: Option<CudaStream>,
) -> Result<()> {
    let stream = stream.map_or(core::ptr::null_mut(), CudaStream::as_raw);
    let memcpy_async = |destination, source, count, kind, stream| unsafe {
        cuda_runtime_api::cudaMemcpyAsync(destination, source, count, kind, stream)
    };
    let stream_synchronize = |stream| unsafe { cuda_runtime_api::cudaStreamSynchronize(stream) };

    // The closures keep the production FFI calls visible and direct while the
    // small generic helper makes ordering and failure behavior testable without
    // introducing a runtime API table or owning any CUDA resources.
    unsafe {
        copy_device_to_shared_memory_with(
            host_destination,
            device_source,
            byte_len,
            stream,
            memcpy_async,
            stream_synchronize,
        )
    }
}

unsafe fn copy_device_to_shared_memory_with<Memcpy, Synchronize>(
    host_destination: *mut u8,
    device_source: *const c_void,
    byte_len: usize,
    stream: *mut c_void,
    mut memcpy_async: Memcpy,
    mut stream_synchronize: Synchronize,
) -> Result<()>
where
    Memcpy: FnMut(*mut c_void, *const c_void, usize, i32, *mut c_void) -> cudaError_t,
    Synchronize: FnMut(*mut c_void) -> cudaError_t,
{
    debug_assert!(!host_destination.is_null());
    debug_assert!(!device_source.is_null());
    debug_assert_ne!(byte_len, 0);

    // SAFETY: The backend keeps this writable shared-memory slot reserved for
    // `byte_len` bytes until synchronization. The public caller guarantees that
    // `device_source` is a compatible readable CUDA range and `stream` is valid.
    memcpy_async(
        host_destination.cast(),
        device_source,
        byte_len,
        CUDA_MEMCPY_DEVICE_TO_HOST,
        stream,
    )
    .check("cudaMemcpyAsync")?;

    // SAFETY: `stream` is the exact handle used for the preceding D2H copy. The
    // backend cannot publish or release its slot until this wait succeeds.
    stream_synchronize(stream).check("cudaStreamSynchronize")
}

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
                Err(Error::Cuda {
                    operation,
                    code: self.0,
                    message: None,
                })
            }
        }
    }

    // CUDA's CUDARTAPI is __stdcall under MSVC. Rust's `system` ABI selects that
    // convention on 32-bit Windows and the platform C ABI on 64-bit Windows.
    #[link(name = "cudart")]
    unsafe extern "system" {
        pub(super) fn cudaMemcpyAsync(
            destination: *mut c_void,
            source: *const c_void,
            count: usize,
            kind: i32,
            stream: *mut c_void,
        ) -> cudaError_t;
        pub(super) fn cudaStreamSynchronize(stream: *mut c_void) -> cudaError_t;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::Error;

    #[link(name = "cudart")]
    unsafe extern "system" {
        fn cudaMalloc(device_pointer: *mut *mut c_void, size: usize) -> cudaError_t;
        fn cudaFree(device_pointer: *mut c_void) -> cudaError_t;
        fn cudaMemcpy(
            destination: *mut c_void,
            source: *const c_void,
            count: usize,
            kind: i32,
        ) -> cudaError_t;
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Copy {
            destination: usize,
            source: usize,
            count: usize,
            kind: i32,
            stream: usize,
        },
        Synchronize {
            stream: usize,
        },
    }

    #[test]
    fn copies_directly_to_supplied_shared_pointer_then_synchronizes_same_stream() {
        let calls = RefCell::new(Vec::new());
        let source = [1_u8, 2, 3, 4];
        let mut shared_slot = [0_u8; 4];
        let stream = 0x1234_usize as *mut c_void;

        // SAFETY: This test mock treats a host array as device-readable memory and
        // copies only within the two live four-byte arrays.
        unsafe {
            copy_device_to_shared_memory_with(
                shared_slot.as_mut_ptr(),
                source.as_ptr().cast(),
                source.len(),
                stream,
                |destination, source, count, kind, stream| {
                    calls.borrow_mut().push(Call::Copy {
                        destination: destination as usize,
                        source: source as usize,
                        count,
                        kind,
                        stream: stream as usize,
                    });
                    core::ptr::copy_nonoverlapping(
                        source.cast::<u8>(),
                        destination.cast::<u8>(),
                        count,
                    );
                    cudaError_t(0)
                },
                |stream| {
                    calls.borrow_mut().push(Call::Synchronize {
                        stream: stream as usize,
                    });
                    cudaError_t(0)
                },
            )
            .expect("direct D2H succeeds");
        }

        assert_eq!(shared_slot, source);
        assert_eq!(
            *calls.borrow(),
            vec![
                Call::Copy {
                    destination: shared_slot.as_mut_ptr() as usize,
                    source: source.as_ptr() as usize,
                    count: source.len(),
                    kind: CUDA_MEMCPY_DEVICE_TO_HOST,
                    stream: stream as usize,
                },
                Call::Synchronize {
                    stream: stream as usize,
                },
            ]
        );
    }

    #[test]
    fn null_stream_is_forwarded_unchanged() {
        let source = [1_u8];
        let mut destination = [0_u8];

        // SAFETY: Mock pointers each cover the requested one byte.
        unsafe {
            copy_device_to_shared_memory_with(
                destination.as_mut_ptr(),
                source.as_ptr().cast(),
                1,
                core::ptr::null_mut(),
                |destination, source, count, _, stream| {
                    assert!(stream.is_null());
                    core::ptr::copy_nonoverlapping(
                        source.cast::<u8>(),
                        destination.cast::<u8>(),
                        count,
                    );
                    cudaError_t(0)
                },
                |stream| {
                    assert!(stream.is_null());
                    cudaError_t(0)
                },
            )
            .expect("null stream transfer succeeds");
        }
    }

    #[test]
    fn copy_failure_does_not_synchronize() {
        let source = [0_u8];
        let mut destination = [0_u8];
        let synchronized = core::cell::Cell::new(false);

        // SAFETY: The failing mock does not dereference either pointer.
        let error = unsafe {
            copy_device_to_shared_memory_with(
                destination.as_mut_ptr(),
                source.as_ptr().cast(),
                1,
                core::ptr::null_mut(),
                |_, _, _, _, _| cudaError_t(17),
                |_| {
                    synchronized.set(true);
                    cudaError_t(0)
                },
            )
        }
        .expect_err("copy must fail");

        assert!(!synchronized.get());
        assert_eq!(
            error,
            Error::Cuda {
                operation: "cudaMemcpyAsync",
                code: 17,
                message: None,
            }
        );
    }

    #[test]
    fn synchronization_failure_is_returned_after_copy() {
        let source = [9_u8];
        let mut destination = [0_u8];

        // SAFETY: Mock pointers each cover the requested one byte.
        let error = unsafe {
            copy_device_to_shared_memory_with(
                destination.as_mut_ptr(),
                source.as_ptr().cast(),
                1,
                core::ptr::null_mut(),
                |destination, source, count, _, _| {
                    core::ptr::copy_nonoverlapping(
                        source.cast::<u8>(),
                        destination.cast::<u8>(),
                        count,
                    );
                    cudaError_t(0)
                },
                |_| cudaError_t(29),
            )
        }
        .expect_err("synchronization must fail");

        assert_eq!(destination, source);
        assert_eq!(
            error,
            Error::Cuda {
                operation: "cudaStreamSynchronize",
                code: 29,
                message: None,
            }
        );
    }

    #[test]
    #[ignore = "requires a CUDA-capable device and Runtime"]
    fn real_cuda_d2h_writes_directly_to_a_windows_shared_mapping() {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE},
        };

        use crate::platform::windows::{MappedView, OwnedHandle};

        const MAPPING_SIZE: u32 = 4096;
        const DESTINATION_OFFSET: usize = 32;
        const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;

        // SAFETY: An unnamed page-file-backed mapping with read/write protection.
        let raw_mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                0,
                MAPPING_SIZE,
                core::ptr::null(),
            )
        };
        assert!(!raw_mapping.is_null(), "CreateFileMappingW failed");
        // SAFETY: Successful creation returned one uniquely owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(raw_mapping) };
        let view = MappedView::map(
            mapping.raw(),
            FILE_MAP_ALL_ACCESS,
            "MapViewOfFile(real CUDA test)",
        )
        .expect("map CUDA test shared memory");

        let expected = [
            0x10_u8, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0,
        ];
        let mut device_pointer = core::ptr::null_mut();
        // SAFETY: Writable pointer storage receives an allocation of exact size.
        unsafe { cudaMalloc(&mut device_pointer, expected.len()) }
            .check("cudaMalloc")
            .expect("allocate real CUDA test source");
        assert!(!device_pointer.is_null());

        // SAFETY: The destination allocation and source array each contain the
        // requested bytes, and kind 1 is cudaMemcpyHostToDevice.
        unsafe {
            cudaMemcpy(
                device_pointer,
                expected.as_ptr().cast(),
                expected.len(),
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        }
        .check("cudaMemcpy")
        .expect("initialize real CUDA test source");

        // SAFETY: The mapped destination contains this entire range; the CUDA
        // allocation is contiguous and readable until the synchronized call ends.
        unsafe {
            copy_device_to_shared_memory(
                view.as_ptr().add(DESTINATION_OFFSET),
                device_pointer,
                expected.len(),
                None,
            )
        }
        .expect("copy CUDA data directly to shared mapping");

        // SAFETY: Synchronization completed and the mapped range is still live.
        assert_eq!(
            unsafe {
                core::slice::from_raw_parts(view.as_ptr().add(DESTINATION_OFFSET), expected.len())
            },
            expected
        );

        // SAFETY: Pointer came from the successful cudaMalloc above and is freed once.
        unsafe { cudaFree(device_pointer) }
            .check("cudaFree")
            .expect("release real CUDA test source");
    }
}
