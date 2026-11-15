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

    let runtime = cuda_runtime_api::runtime()?;
    let stream = stream.map_or(core::ptr::null_mut(), CudaStream::as_raw);
    // SAFETY: The backend reserves a writable shared-memory range of `byte_len`
    // bytes until synchronization. The public caller guarantees a compatible,
    // readable CUDA source range and a valid stream/current context.
    unsafe { (runtime.memcpy_async)(host_destination.cast(), device_source, byte_len, CUDA_MEMCPY_DEVICE_TO_HOST, stream) }.check("cudaMemcpyAsync")?;

    // SAFETY: This is the exact stream used by the preceding D2H operation.
    unsafe { (runtime.stream_synchronize)(stream) }.check("cudaStreamSynchronize")
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
    pub(super) type MemcpyAsync = unsafe extern "system" fn(*mut c_void, *const c_void, usize, i32, *mut c_void) -> cudaError_t;
    pub(super) type StreamSynchronize = unsafe extern "system" fn(*mut c_void) -> cudaError_t;

    /// The two CUDA Runtime entry points the D2H path needs.
    pub(super) struct Runtime {
        pub(super) memcpy_async: MemcpyAsync,
        pub(super) stream_synchronize: StreamSynchronize,
    }

    /// CUDA Runtime DLLs, newest first.
    ///
    /// The crate links no CUDA library: a library-chosen link would either
    /// duplicate the symbols of an application that links the Runtime itself
    /// (CUDA 13's `cudart.lib` is a static loader, not an import library) or pin
    /// one CUDA major version. The application has already loaded and
    /// initialized its Runtime before passing device pointers, so the module it
    /// loaded is preferred; otherwise the standard DLL search loads one.
    const MODULES: [&str; 4] = ["cudart64_13.dll", "cudart64_12.dll", "cudart64_110.dll", "cudart64_101.dll"];

    static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();

    /// Resolves (once) the CUDA Runtime used for D2H copies.
    pub(super) fn runtime() -> Result<&'static Runtime> {
        if let Some(runtime) = RUNTIME.get() {
            return Ok(runtime);
        }
        let module = module()?;
        let runtime = Runtime {
            // SAFETY: The exported symbols have exactly these CUDA Runtime signatures.
            memcpy_async: unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, MemcpyAsync>(symbol(module, c"cudaMemcpyAsync")?) },
            // SAFETY: As above.
            stream_synchronize: unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, StreamSynchronize>(symbol(module, c"cudaStreamSynchronize")?) },
        };
        Ok(RUNTIME.get_or_init(|| runtime))
    }

    /// Finds a loaded CUDA Runtime (pinned so it cannot unload under cached
    /// function pointers), else loads one through the standard search order.
    pub(super) fn module() -> Result<windows_sys::Win32::Foundation::HMODULE> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::LibraryLoader::{GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW, LoadLibraryW};

        let wide = |name: &str| name.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        for name in MODULES {
            let mut module = core::ptr::null_mut();
            // SAFETY: NUL-terminated name and writable out pointer; PIN keeps the
            // module loaded for the remaining process lifetime.
            if unsafe { GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN, wide(name).as_ptr(), &mut module) } != 0 && !module.is_null() {
                return Ok(module);
            }
        }
        for name in MODULES {
            // SAFETY: NUL-terminated name; the loaded module is intentionally never freed.
            let module = unsafe { LoadLibraryW(wide(name).as_ptr()) };
            if !module.is_null() {
                return Ok(module);
            }
        }
        // SAFETY: Reads the calling thread's last error after the failed loads.
        Err(Error::Windows {
            operation: "LoadLibraryW(cudart64_*.dll)",
            code: unsafe { GetLastError() },
        })
    }

    /// Looks up one exported function of the resolved module.
    pub(super) fn symbol(module: windows_sys::Win32::Foundation::HMODULE, name: &core::ffi::CStr) -> Result<unsafe extern "system" fn() -> isize> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;

        // SAFETY: `module` is a loaded, pinned or never-freed module and `name` is NUL-terminated.
        unsafe { GetProcAddress(module, name.as_ptr().cast()) }.ok_or_else(|| Error::Windows {
            operation: "GetProcAddress(CUDA Runtime)",
            // SAFETY: Reads the calling thread's last error after the failed lookup.
            code: unsafe { GetLastError() },
        })
    }
}

/// Resolves one CUDA Runtime entry point for tests that must allocate device
/// memory through the same module the D2H path uses.
///
/// # Safety
///
/// `F` must be the exact signature of the named export.
#[cfg(all(test, windows))]
pub(crate) unsafe fn test_api<F: Copy>(name: &core::ffi::CStr) -> F {
    let module = cuda_runtime_api::module().expect("CUDA Runtime DLL");
    let symbol = cuda_runtime_api::symbol(module, name).expect("CUDA Runtime export");
    assert_eq!(size_of::<F>(), size_of_val(&symbol));
    // SAFETY: The caller names the export's exact function-pointer type.
    unsafe { core::mem::transmute_copy(&symbol) }
}

#[cfg(all(test, windows))]
mod tests {
    use super::cuda_runtime_api::cudaError_t;
    use super::*;
    use crate::Error;

    /// Resolves a test-only CUDA Runtime entry point from the same module as the D2H path.
    ///
    /// # Safety
    /// `F` must be the exact signature of the named export.
    unsafe fn api<F: Copy>(name: &core::ffi::CStr) -> F {
        // SAFETY: Forwarded caller contract.
        unsafe { test_api(name) }
    }

    #[test]
    fn missing_runtime_exports_report_the_windows_error() {
        let module = super::cuda_runtime_api::module();
        if let Ok(module) = module {
            assert!(matches!(super::cuda_runtime_api::symbol(module, c"virtualcamNoSuchExport"), Err(Error::Windows { .. })));
        } else {
            assert!(matches!(module, Err(Error::Windows { .. })), "no CUDA Runtime installed: load failure is a Windows error");
        }
    }

    #[test]
    fn cuda_errors_preserve_operation_and_exact_code() {
        assert_eq!(cudaError_t(17).check("cudaMemcpyAsync"), Err(Error::Cuda { operation: "cudaMemcpyAsync", code: 17 }));
    }

    #[test]
    #[ignore = "requires a CUDA-capable device and Runtime"]
    #[allow(non_snake_case, reason = "local bindings keep the CUDA Runtime API names")]
    fn real_cuda_d2h_writes_directly_to_a_windows_shared_mapping() {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, PAGE_READWRITE};

        use crate::platform::windows::{MappedView, OwnedHandle};

        type Malloc = unsafe extern "system" fn(*mut *mut c_void, usize) -> cudaError_t;
        type Free = unsafe extern "system" fn(*mut c_void) -> cudaError_t;
        type Memcpy = unsafe extern "system" fn(*mut c_void, *const c_void, usize, i32) -> cudaError_t;
        type StreamCreate = unsafe extern "system" fn(*mut *mut c_void, u32) -> cudaError_t;
        type StreamDestroy = unsafe extern "system" fn(*mut c_void) -> cudaError_t;
        // SAFETY: Each type is the documented CUDA Runtime signature of its export.
        let (cudaMalloc, cudaFree, cudaMemcpy, cudaStreamCreateWithFlags, cudaStreamDestroy) = unsafe {
            (
                api::<Malloc>(c"cudaMalloc"),
                api::<Free>(c"cudaFree"),
                api::<Memcpy>(c"cudaMemcpy"),
                api::<StreamCreate>(c"cudaStreamCreateWithFlags"),
                api::<StreamDestroy>(c"cudaStreamDestroy"),
            )
        };

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
