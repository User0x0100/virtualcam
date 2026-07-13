use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
};

use crate::{Error, Result};

const CUDA_SUCCESS: i32 = 0;
const CUDA_MEMCPY_DEVICE_TO_HOST: i32 = 2;
const CUDA_HOST_ALLOC_DEFAULT: u32 = 0;

/// An opaque CUDA Runtime stream handle borrowed from the caller.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CudaStream(*mut c_void);

impl CudaStream {
    /// Wraps a raw `cudaStream_t` without taking ownership.
    ///
    /// # Safety
    ///
    /// `raw` must be a live CUDA Runtime stream compatible with the device
    /// pointer and current context used for every call that receives this value.
    /// The caller keeps the stream alive and destroys it only after outstanding
    /// camera writes finish. A null value is accepted, but `None` more clearly
    /// selects the CUDA default stream.
    pub const unsafe fn from_raw(raw: *mut c_void) -> Self {
        Self(raw)
    }

    /// Returns the borrowed raw `cudaStream_t`.
    pub const fn as_raw(self) -> *mut c_void {
        self.0
    }
}

#[derive(Clone, Copy)]
struct CudaApi {
    host_alloc: unsafe fn(*mut *mut c_void, usize, u32) -> i32,
    free_host: unsafe fn(*mut c_void) -> i32,
    memcpy_async: unsafe fn(*mut c_void, *const c_void, usize, i32, *mut c_void) -> i32,
    stream_synchronize: unsafe fn(*mut c_void) -> i32,
    error_string: unsafe fn(i32) -> *const c_char,
}

impl CudaApi {
    const SYSTEM: Self = Self {
        host_alloc: system_host_alloc,
        free_host: system_free_host,
        memcpy_async: system_memcpy_async,
        stream_synchronize: system_stream_synchronize,
        error_string: system_error_string,
    };
}

pub(crate) struct CudaStaging {
    pointer: Option<NonNull<u8>>,
    len: usize,
    api: CudaApi,
}

impl CudaStaging {
    fn allocate(len: usize, api: CudaApi) -> Result<Self> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: `raw` is writable pointer storage, `len` is the validated frame
        // length, and default flags request a portable host-only staging allocation.
        let code = unsafe { (api.host_alloc)(&mut raw, len, CUDA_HOST_ALLOC_DEFAULT) };
        if code != CUDA_SUCCESS {
            return Err(cuda_error(api, "cudaHostAlloc", code));
        }
        let Some(pointer) = NonNull::new(raw.cast::<u8>()) else {
            return Err(Error::Cuda {
                operation: "cudaHostAlloc",
                code: CUDA_SUCCESS,
                message: Some("CUDA reported success but returned a null pointer".to_owned()),
            });
        };
        trace_debug!(len, "allocated CUDA pinned staging buffer");
        Ok(Self {
            pointer: Some(pointer),
            len,
            api,
        })
    }

    unsafe fn copy_from_device(
        &mut self,
        device_pointer: *const c_void,
        stream: Option<CudaStream>,
    ) -> Result<&[u8]> {
        let destination = self.pointer.ok_or(Error::Cuda {
            operation: "cudaMemcpyAsync",
            code: -1,
            message: Some("pinned staging buffer was already released".to_owned()),
        })?;
        let raw_stream = stream.map_or(std::ptr::null_mut(), CudaStream::as_raw);
        // SAFETY: The caller guarantees `device_pointer` is readable for
        // `self.len` bytes on `raw_stream`; destination is our live pinned
        // allocation of exactly that length. D2H kind is CUDA's enum value 2.
        let copy_code = unsafe {
            (self.api.memcpy_async)(
                destination.as_ptr().cast(),
                device_pointer,
                self.len,
                CUDA_MEMCPY_DEVICE_TO_HOST,
                raw_stream,
            )
        };
        if copy_code != CUDA_SUCCESS {
            return Err(cuda_error(self.api, "cudaMemcpyAsync", copy_code));
        }

        // SAFETY: `raw_stream` is either the caller's live stream or null for the
        // CUDA default stream. Waiting only this stream completes the preceding D2H.
        let sync_code = unsafe { (self.api.stream_synchronize)(raw_stream) };
        if sync_code != CUDA_SUCCESS {
            return Err(cuda_error(self.api, "cudaStreamSynchronize", sync_code));
        }

        // SAFETY: Synchronization completed the full D2H into our allocation,
        // which remains live and immutable through the returned shared slice.
        Ok(unsafe { core::slice::from_raw_parts(destination.as_ptr(), self.len) })
    }

    pub(crate) fn release(&mut self) -> Result<()> {
        let Some(pointer) = self.pointer.take() else {
            return Ok(());
        };
        // SAFETY: Pointer came from this API table's successful cudaHostAlloc and
        // is consumed exactly once. It is cleared even if a broken runtime cannot
        // complete the release, preventing a panic or repeated invalid call.
        let code = unsafe { (self.api.free_host)(pointer.as_ptr().cast()) };
        if code == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(cuda_error(self.api, "cudaFreeHost", code))
        }
    }
}

impl Drop for CudaStaging {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

pub(crate) unsafe fn transfer_then_publish<T>(
    staging: &mut Option<CudaStaging>,
    frame_size: usize,
    device_pointer: *const c_void,
    stream: Option<CudaStream>,
    publish: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    // SAFETY: Forwarded from VirtualCamera::write_cuda after exact byte-length
    // validation; SYSTEM uses the declared CUDA Runtime entry points.
    unsafe {
        transfer_then_publish_with_api(
            staging,
            frame_size,
            device_pointer,
            stream,
            CudaApi::SYSTEM,
            publish,
        )
    }
}

unsafe fn transfer_then_publish_with_api<T>(
    staging: &mut Option<CudaStaging>,
    frame_size: usize,
    device_pointer: *const c_void,
    stream: Option<CudaStream>,
    api: CudaApi,
    publish: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    if staging.is_none() {
        *staging = Some(CudaStaging::allocate(frame_size, api)?);
    }
    let staging = staging.as_mut().ok_or(Error::Cuda {
        operation: "cudaHostAlloc",
        code: -1,
        message: Some("staging allocation was not retained".to_owned()),
    })?;
    if staging.len != frame_size {
        return Err(Error::Protocol {
            backend: crate::CameraType::Obs,
            detail: "CUDA staging size differs from camera frame size",
        });
    }
    // SAFETY: The caller provides the CUDA pointer/stream validity contract;
    // allocation and stream synchronization are completed before publication.
    let host_frame = unsafe { staging.copy_from_device(device_pointer, stream)? };
    publish(host_frame)
}

fn cuda_error(api: CudaApi, operation: &'static str, code: i32) -> Error {
    // SAFETY: CUDA accepts every cudaError_t value and returns a static string or
    // null. The pointer, when non-null, remains valid for this immediate copy.
    let message_pointer = unsafe { (api.error_string)(code) };
    let message = if message_pointer.is_null() {
        None
    } else {
        // SAFETY: A non-null cudaGetErrorString result is a NUL-terminated static
        // C string owned by the CUDA Runtime.
        Some(
            unsafe { CStr::from_ptr(message_pointer) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    trace_warn!(operation, code, message = ?message, "CUDA Runtime operation failed");
    Error::Cuda {
        operation,
        code,
        message,
    }
}

unsafe fn system_host_alloc(pointer: *mut *mut c_void, len: usize, flags: u32) -> i32 {
    // SAFETY: Preconditions are supplied by CudaStaging::allocate.
    unsafe { ffi::cuda_host_alloc(pointer, len, flags) }
}

unsafe fn system_free_host(pointer: *mut c_void) -> i32 {
    // SAFETY: Preconditions are supplied by CudaStaging::release.
    unsafe { ffi::cuda_free_host(pointer) }
}

unsafe fn system_memcpy_async(
    destination: *mut c_void,
    source: *const c_void,
    len: usize,
    kind: i32,
    stream: *mut c_void,
) -> i32 {
    // SAFETY: Preconditions are supplied by CudaStaging::copy_from_device.
    unsafe { ffi::cuda_memcpy_async(destination, source, len, kind, stream) }
}

unsafe fn system_stream_synchronize(stream: *mut c_void) -> i32 {
    // SAFETY: Preconditions are supplied by CudaStaging::copy_from_device.
    unsafe { ffi::cuda_stream_synchronize(stream) }
}

unsafe fn system_error_string(code: i32) -> *const c_char {
    // SAFETY: CUDA defines this operation for cudaError_t values.
    unsafe { ffi::cuda_get_error_string(code) }
}

mod ffi {
    use super::{c_char, c_void};

    #[cfg(windows)]
    #[cfg_attr(not(test), link(name = "cudart"))]
    unsafe extern "system" {
        #[link_name = "cudaHostAlloc"]
        pub(super) fn cuda_host_alloc(pointer: *mut *mut c_void, size: usize, flags: u32) -> i32;
        #[link_name = "cudaFreeHost"]
        pub(super) fn cuda_free_host(pointer: *mut c_void) -> i32;
        #[link_name = "cudaMemcpyAsync"]
        pub(super) fn cuda_memcpy_async(
            destination: *mut c_void,
            source: *const c_void,
            count: usize,
            kind: i32,
            stream: *mut c_void,
        ) -> i32;
        #[link_name = "cudaStreamSynchronize"]
        pub(super) fn cuda_stream_synchronize(stream: *mut c_void) -> i32;
        #[link_name = "cudaGetErrorString"]
        pub(super) fn cuda_get_error_string(error: i32) -> *const c_char;
    }

    #[cfg(not(windows))]
    #[cfg_attr(not(test), link(name = "cudart"))]
    unsafe extern "C" {
        #[link_name = "cudaHostAlloc"]
        pub(super) fn cuda_host_alloc(pointer: *mut *mut c_void, size: usize, flags: u32) -> i32;
        #[link_name = "cudaFreeHost"]
        pub(super) fn cuda_free_host(pointer: *mut c_void) -> i32;
        #[link_name = "cudaMemcpyAsync"]
        pub(super) fn cuda_memcpy_async(
            destination: *mut c_void,
            source: *const c_void,
            count: usize,
            kind: i32,
            stream: *mut c_void,
        ) -> i32;
        #[link_name = "cudaStreamSynchronize"]
        pub(super) fn cuda_stream_synchronize(stream: *mut c_void) -> i32;
        #[link_name = "cudaGetErrorString"]
        pub(super) fn cuda_get_error_string(error: i32) -> *const c_char;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        HostAlloc,
        Memcpy { stream: usize },
        Synchronize { stream: usize },
        Publish,
        FreeHost,
    }

    struct MockState {
        calls: Vec<Call>,
        allocation: Option<Box<[u8]>>,
        memcpy_error: i32,
        sync_error: i32,
    }

    impl MockState {
        const fn new() -> Self {
            Self {
                calls: Vec::new(),
                allocation: None,
                memcpy_error: 0,
                sync_error: 0,
            }
        }
    }

    static SERIAL: Mutex<()> = Mutex::new(());
    static STATE: Mutex<MockState> = Mutex::new(MockState::new());

    fn reset() {
        *STATE.lock().expect("mock state lock") = MockState::new();
    }

    unsafe fn host_alloc(pointer: *mut *mut c_void, len: usize, _flags: u32) -> i32 {
        let mut allocation = vec![0_u8; len].into_boxed_slice();
        let raw = allocation.as_mut_ptr();
        let mut state = STATE.lock().expect("mock state lock");
        state.calls.push(Call::HostAlloc);
        state.allocation = Some(allocation);
        // SAFETY: Test passes writable pointer storage to this mock API.
        unsafe { pointer.write(raw.cast()) };
        0
    }

    unsafe fn free_host(_pointer: *mut c_void) -> i32 {
        let mut state = STATE.lock().expect("mock state lock");
        state.calls.push(Call::FreeHost);
        state.allocation = None;
        0
    }

    unsafe fn memcpy_async(
        destination: *mut c_void,
        source: *const c_void,
        len: usize,
        _kind: i32,
        stream: *mut c_void,
    ) -> i32 {
        let mut state = STATE.lock().expect("mock state lock");
        state.calls.push(Call::Memcpy {
            stream: stream as usize,
        });
        if state.memcpy_error != 0 {
            return state.memcpy_error;
        }
        drop(state);
        // SAFETY: Test source and mock allocation each contain `len` bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(source.cast::<u8>(), destination.cast::<u8>(), len)
        };
        0
    }

    unsafe fn synchronize(stream: *mut c_void) -> i32 {
        let mut state = STATE.lock().expect("mock state lock");
        state.calls.push(Call::Synchronize {
            stream: stream as usize,
        });
        state.sync_error
    }

    unsafe fn error_string(_code: i32) -> *const c_char {
        c"mock CUDA error".as_ptr()
    }

    const MOCK_API: CudaApi = CudaApi {
        host_alloc,
        free_host,
        memcpy_async,
        stream_synchronize: synchronize,
        error_string,
    };

    #[test]
    fn default_and_supplied_streams_are_forwarded_and_staging_is_reused() {
        let _serial = SERIAL.lock().expect("serial test lock");
        reset();
        let source = [1_u8, 2, 3, 4];
        let mut staging = None;

        // SAFETY: Mock treats this host array as readable device memory.
        unsafe {
            transfer_then_publish_with_api(
                &mut staging,
                source.len(),
                source.as_ptr().cast(),
                None,
                MOCK_API,
                |frame| {
                    assert_eq!(frame, source);
                    STATE
                        .lock()
                        .expect("mock state lock")
                        .calls
                        .push(Call::Publish);
                    Ok(())
                },
            )
            .expect("default stream transfer");
        }
        let stream_pointer = 0x1234_usize as *mut c_void;
        // SAFETY: Mock accepts the sentinel stream and host source pointer.
        unsafe {
            transfer_then_publish_with_api(
                &mut staging,
                source.len(),
                source.as_ptr().cast(),
                Some(CudaStream::from_raw(stream_pointer)),
                MOCK_API,
                |_| {
                    STATE
                        .lock()
                        .expect("mock state lock")
                        .calls
                        .push(Call::Publish);
                    Ok(())
                },
            )
            .expect("supplied stream transfer");
        }

        let calls = STATE.lock().expect("mock state lock").calls.clone();
        assert_eq!(
            calls,
            vec![
                Call::HostAlloc,
                Call::Memcpy { stream: 0 },
                Call::Synchronize { stream: 0 },
                Call::Publish,
                Call::Memcpy {
                    stream: stream_pointer as usize
                },
                Call::Synchronize {
                    stream: stream_pointer as usize
                },
                Call::Publish,
            ]
        );
        staging
            .as_mut()
            .expect("staging allocated")
            .release()
            .expect("free staging");
    }

    #[test]
    fn memcpy_failure_does_not_synchronize_or_publish() {
        let _serial = SERIAL.lock().expect("serial test lock");
        reset();
        STATE.lock().expect("mock state lock").memcpy_error = 17;
        let source = [0_u8; 4];
        let mut staging = None;
        // SAFETY: Mock accepts the host pointer as device memory.
        let error = unsafe {
            transfer_then_publish_with_api(
                &mut staging,
                source.len(),
                source.as_ptr().cast(),
                None,
                MOCK_API,
                |_| {
                    STATE
                        .lock()
                        .expect("mock state lock")
                        .calls
                        .push(Call::Publish);
                    Ok(())
                },
            )
        }
        .expect_err("copy must fail");
        assert!(matches!(
            error,
            Error::Cuda {
                operation: "cudaMemcpyAsync",
                code: 17,
                ..
            }
        ));
        let calls = STATE.lock().expect("mock state lock").calls.clone();
        assert_eq!(calls, vec![Call::HostAlloc, Call::Memcpy { stream: 0 }]);
    }

    #[test]
    fn synchronize_failure_does_not_publish() {
        let _serial = SERIAL.lock().expect("serial test lock");
        reset();
        STATE.lock().expect("mock state lock").sync_error = 29;
        let source = [0_u8; 4];
        let mut staging = None;
        // SAFETY: Mock accepts the host pointer as device memory.
        let error = unsafe {
            transfer_then_publish_with_api(
                &mut staging,
                source.len(),
                source.as_ptr().cast(),
                None,
                MOCK_API,
                |_| {
                    STATE
                        .lock()
                        .expect("mock state lock")
                        .calls
                        .push(Call::Publish);
                    Ok(())
                },
            )
        }
        .expect_err("sync must fail");
        assert!(matches!(
            error,
            Error::Cuda {
                operation: "cudaStreamSynchronize",
                code: 29,
                ..
            }
        ));
        let calls = STATE.lock().expect("mock state lock").calls.clone();
        assert_eq!(
            calls,
            vec![
                Call::HostAlloc,
                Call::Memcpy { stream: 0 },
                Call::Synchronize { stream: 0 },
            ]
        );
    }
}
