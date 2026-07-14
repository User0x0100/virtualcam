use std::{marker::PhantomData, rc::Rc};

#[cfg(windows)]
use crate::backend::Backend;
use crate::{CameraType, Error, ObsOptions, PixelFormat, Result, UnityCaptureOptions, WriteStatus};

#[cfg(feature = "cuda")]
use crate::cuda::CudaStream;
#[cfg(all(feature = "cuda", windows))]
use crate::cuda::{self, CudaStaging};

/// Unified producer facade for a selected virtual-camera backend.
///
/// Instances are intentionally neither `Send` nor `Sync`: writes are serialized
/// through `&mut self`, and CUDA context affinity remains the caller's concern.
pub struct VirtualCamera {
    camera_type: CameraType,
    native_format: PixelFormat,
    frame_size: usize,
    closed: bool,
    #[cfg(windows)]
    backend: Backend,
    #[cfg(all(feature = "cuda", windows))]
    cuda_staging: Option<CudaStaging>,
    not_send_or_sync: PhantomData<Rc<()>>,
}

impl VirtualCamera {
    /// Opens an OBS Virtual Camera producer with OBS-specific controls.
    ///
    /// OBS creates its fixed shared queue immediately and reports
    /// [`Error::DeviceInUse`] when another mapping exists, unless the caller
    /// explicitly accepts [`ObsOptions::force_open`]'s contract.
    pub fn open_obs(options: ObsOptions) -> Result<Self> {
        #[cfg(not(windows))]
        {
            let _ = options;
            Err(Error::UnsupportedPlatform)
        }

        #[cfg(windows)]
        {
            let validated = options.validate()?;
            let backend = Backend::open_obs(validated)?;
            trace_debug!(
                camera_type = ?CameraType::Obs,
                width = validated.width,
                height = validated.height,
                interval_100ns = validated.interval_100ns,
                "virtual camera opened"
            );
            Ok(Self {
                camera_type: CameraType::Obs,
                native_format: PixelFormat::Nv12,
                frame_size: validated.frame_size,
                closed: false,
                backend,
                #[cfg(all(feature = "cuda", windows))]
                cuda_staging: None,
                not_send_or_sync: PhantomData,
            })
        }
    }

    /// Opens a Unity Capture producer with Unity-specific protocol controls.
    ///
    /// Opening only creates local sender state. The selected Unity Capture
    /// DirectShow device may become active later; use [`Self::is_ready`] or
    /// inspect [`WriteStatus::ConsumerInactive`].
    pub fn open_unity_capture(options: UnityCaptureOptions) -> Result<Self> {
        #[cfg(not(windows))]
        {
            let _ = options;
            Err(Error::UnsupportedPlatform)
        }

        #[cfg(windows)]
        {
            let validated = options.validate()?;
            let backend = Backend::open_unity_capture(validated)?;
            trace_debug!(
                camera_type = ?CameraType::UnityCapture,
                capture_number = validated.capture_number,
                width = validated.width,
                height = validated.height,
                format = ?validated.format,
                "virtual camera opened"
            );
            Ok(Self {
                camera_type: CameraType::UnityCapture,
                native_format: validated.format.pixel_format(),
                frame_size: validated.frame_size,
                closed: false,
                backend,
                #[cfg(all(feature = "cuda", windows))]
                cuda_staging: None,
                not_send_or_sync: PhantomData,
            })
        }
    }

    /// Returns the selected backend protocol.
    pub const fn camera_type(&self) -> CameraType {
        self.camera_type
    }

    /// Returns the native format required by this backend.
    pub const fn native_format(&self) -> PixelFormat {
        self.native_format
    }

    /// Returns the exact number of bytes required for each frame.
    ///
    /// This includes Unity row padding when `stride_pixels` was explicitly set.
    pub const fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Reports whether this local facade has not been closed.
    ///
    /// It does not imply that a consumer is active; use [`Self::is_ready`].
    pub const fn is_open(&self) -> bool {
        !self.closed
    }

    /// Reports whether the backend protocol can currently accept a frame.
    ///
    /// Unity Capture returns `false` until receiver-created objects exist. OBS
    /// has no consumer-presence handshake, so an open producer is ready even if
    /// no application currently reads the camera.
    pub fn is_ready(&mut self) -> Result<bool> {
        if self.closed {
            return Err(Error::AlreadyClosed);
        }
        #[cfg(not(windows))]
        {
            Err(Error::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            self.backend.is_ready()
        }
    }

    /// Publishes one host-memory frame.
    ///
    /// `frame` must use [`Self::native_format`], include any configured Unity
    /// row padding, and contain exactly [`Self::frame_size`] bytes. `Written`
    /// means the full frame has
    /// been copied and the backend's publication fields or event were updated.
    pub fn write_host(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        if self.closed {
            return Err(Error::AlreadyClosed);
        }
        if frame.len() != self.frame_size {
            return Err(Error::FrameSizeMismatch {
                expected: self.frame_size,
                actual: frame.len(),
            });
        }
        #[cfg(not(windows))]
        {
            let _ = frame;
            Err(Error::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            self.backend.write_host_frame(frame)
        }
    }

    /// Copies one device frame to reusable pinned host memory and publishes it.
    ///
    /// `None` passes a null `cudaStream_t` and therefore uses the caller's CUDA
    /// default-stream mode. `Some(stream)` is passed through unchanged. This is
    /// a synchronous publication API: success means D2H completed, the source
    /// pointer may be reused, and the backend protocol has published the frame.
    ///
    /// # Safety
    ///
    /// `device_ptr` must remain readable for `byte_len` bytes until this method
    /// returns and must belong to the CUDA device/context current on this thread.
    /// `byte_len` must equal [`Self::frame_size`]. A supplied stream must be a
    /// live `cudaStream_t` compatible with that pointer and context. The caller
    /// must initialize the CUDA Runtime, select the device, establish/switch any
    /// context, and keep both runtime and stream alive. This crate does none of
    /// those operations and the runtime must remain usable until the camera is
    /// closed so its pinned allocation can be freed.
    #[cfg(feature = "cuda")]
    pub unsafe fn write_cuda(
        &mut self,
        device_ptr: *const core::ffi::c_void,
        byte_len: usize,
        stream: Option<CudaStream>,
    ) -> Result<WriteStatus> {
        if self.closed {
            return Err(Error::AlreadyClosed);
        }
        if byte_len != self.frame_size {
            return Err(Error::FrameSizeMismatch {
                expected: self.frame_size,
                actual: byte_len,
            });
        }
        if device_ptr.is_null() {
            return Err(Error::Protocol {
                backend: self.camera_type,
                detail: "CUDA device pointer is null",
            });
        }

        #[cfg(not(windows))]
        {
            let _ = (device_ptr, stream);
            Err(Error::UnsupportedPlatform)
        }
        #[cfg(windows)]
        {
            let backend = &mut self.backend;
            // SAFETY: The caller upholds the device-pointer, current-context, and
            // stream contracts documented above; `byte_len` was checked against
            // the allocation size passed to CUDA and the backend frame size.
            unsafe {
                cuda::transfer_then_publish(
                    &mut self.cuda_staging,
                    self.frame_size,
                    device_ptr,
                    stream,
                    |host_frame| backend.write_host_frame(host_frame),
                )
            }
        }
    }

    /// Closes all resources owned by this instance.
    ///
    /// Closing an already closed camera succeeds. CUDA deallocation errors are
    /// returned but never panic; backend resources are still closed.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        #[cfg(windows)]
        let backend_result = self.backend.close();
        #[cfg(not(windows))]
        let backend_result: Result<()> = Ok(());

        #[cfg(all(feature = "cuda", windows))]
        let cuda_result = if let Some(staging) = self.cuda_staging.as_mut() {
            let result = staging.release();
            self.cuda_staging = None;
            result
        } else {
            Ok(())
        };
        #[cfg(not(all(feature = "cuda", windows)))]
        let cuda_result: Result<()> = Ok(());

        trace_debug!(camera_type = ?self.camera_type, "virtual camera closed");
        backend_result.and(cuda_result)
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
