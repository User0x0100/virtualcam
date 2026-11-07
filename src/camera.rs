use core::cell::Cell;
use core::marker::PhantomData;

#[cfg(windows)]
use virtualcam_protocol::ValidatedFormat;

#[cfg(windows)]
use crate::ManagedCameraId;
#[cfg(windows)]
use crate::backend::Backend;
#[cfg(feature = "cuda")]
use crate::cuda::CudaStream;
use crate::{CameraType, Error, ObsOptions, PixelFormat, Result, UnityCaptureOptions, VideoFormat, WriteStatus};

/// Unified producer facade for a selected virtual-camera backend.
///
/// A camera is `Send`, so it can be opened on one thread and fed from a
/// dedicated capture/render thread. It is not `Sync`: writes are serialized
/// through `&mut self`. With CUDA, each `write_cuda` call's pointer and stream
/// must belong to the context current on the calling thread.
pub struct VirtualCamera {
    camera_type: CameraType,
    native_format: PixelFormat,
    video_format: Option<VideoFormat>,
    frame_size: usize,
    closed: bool,
    #[cfg(windows)]
    backend: Backend,
    not_sync: PhantomData<Cell<()>>,
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
                width = validated.format.width,
                height = validated.format.height,
                interval_100ns = validated.format.interval_100ns,
                "virtual camera opened"
            );
            Ok(Self {
                camera_type: CameraType::Obs,
                native_format: PixelFormat::Nv12,
                video_format: Some(VideoFormat::new(PixelFormat::Nv12, validated.format.width, validated.format.height, options.frame_rate)),
                frame_size: validated.format.frame_size,
                closed: false,
                backend,
                not_sync: PhantomData,
            })
        }
    }

    /// Opens one managed DirectShow producer session; reached through
    /// [`crate::ManagedCameraManager::open`] after registration checks.
    #[cfg(windows)]
    pub(crate) fn open_managed_dshow(id: ManagedCameraId, format: ValidatedFormat) -> Result<Self> {
        let backend = Backend::open_managed_dshow(id, format)?;
        Ok(Self::managed(CameraType::ManagedDirectShow, id, format, backend))
    }

    /// Opens one managed Media Foundation producer session; reached through
    /// [`crate::ManagedCameraManager::open`] after registration checks.
    #[cfg(windows)]
    pub(crate) fn open_managed_mf(id: ManagedCameraId, format: ValidatedFormat, rendezvous_id: &str) -> Result<Self> {
        let backend = Backend::open_managed_mf(id, format, rendezvous_id)?;
        Ok(Self::managed(CameraType::ManagedMediaFoundation, id, format, backend))
    }

    #[cfg(windows)]
    pub(crate) fn managed(camera_type: CameraType, id: ManagedCameraId, format: ValidatedFormat, backend: Backend) -> Self {
        trace_debug!(camera_type = ?camera_type, camera_id = %id, format = %format.format(), "managed virtual camera opened");
        #[cfg(not(feature = "tracing"))]
        let _ = id;
        Self {
            camera_type,
            native_format: format.format().pixel_format,
            video_format: Some(format.format()),
            frame_size: format.frame_size(),
            closed: false,
            backend,
            not_sync: PhantomData,
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
                video_format: None,
                frame_size: validated.frame_size,
                closed: false,
                backend,
                not_sync: PhantomData,
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

    /// Returns the complete stream format for NV12/managed backends, or `None`
    /// for Unity Capture (whose RGBA formats carry no frame rate or colorimetry).
    pub const fn video_format(&self) -> Option<VideoFormat> {
        self.video_format
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
    /// Unity Capture returns `false` until receiver-created objects exist, and
    /// a managed Media Foundation camera until a client has started it and
    /// this producer authenticated to its source. OBS and managed DirectShow
    /// have no consumer-presence handshake, so an open producer is ready even
    /// if no application currently reads the camera.
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

    /// The format consumers want when it differs from the one this session
    /// writes, or `None` (none pending, closed, or an OBS/Unity camera).
    ///
    /// Managed DirectShow: the format a connected consumer graph negotiated.
    /// A graph's media type is fixed once connected; frames in any other
    /// pixel format reach it as black, so a producer that can convert should
    /// reopen in the requested pixel format. Reading this is a few
    /// shared-memory loads, cheap enough to check before every frame.
    ///
    /// Managed Media Foundation: the mode Frame Server's consumers selected
    /// (the native mode or a conversion the source offers), reported by the
    /// source and polled with the session's liveness check. Until the
    /// producer reopens in it with [`crate::ManagedCameraManager::open`], the
    /// source converts the written mode when it can and shows black otherwise.
    pub fn requested_format(&self) -> Option<VideoFormat> {
        if self.closed {
            return None;
        }
        #[cfg(not(windows))]
        {
            None
        }
        #[cfg(windows)]
        {
            self.backend.requested_format()
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

    /// Copies one contiguous device frame directly into virtual-camera shared
    /// memory and publishes it after the copy stream has synchronized.
    ///
    /// The crate allocates no CUDA or host staging buffer and performs no second
    /// CPU copy. `None` passes a null `cudaStream_t`; `Some(stream)` is forwarded
    /// unchanged. This remains a synchronous publication API: success means the
    /// D2H operation and `cudaStreamSynchronize` completed, the source can be
    /// reused, and the backend protocol published the frame.
    ///
    /// The destination is an unregistered Windows shared-memory mapping. It is a
    /// contiguous virtual byte range, but the crate does not page-lock it. CUDA
    /// may therefore apply its pageable-memory synchronization or internal
    /// staging behavior; callers must not assume transfer/compute overlap.
    ///
    /// # Safety
    ///
    /// `device_ptr..device_ptr + byte_len` must be one readable, contiguous CUDA
    /// device-memory region until this method returns. It must already contain
    /// exactly the backend's byte layout, including all planes and configured row
    /// padding; CUDA arrays, textures, surfaces, and discontiguous planes are not
    /// accepted by this pointer API. `byte_len` must equal [`Self::frame_size`].
    ///
    /// The pointer must belong to the CUDA device/context current on this thread.
    /// A supplied stream must be a live compatible `cudaStream_t`, and all work
    /// producing the frame must precede the D2H copy in that stream (or already be
    /// synchronized). The caller initializes the Runtime, selects the device,
    /// manages context switching, and owns the stream. The Runtime and stream
    /// must remain usable only until this method returns; `close` performs no CUDA
    /// operation.
    #[cfg(feature = "cuda")]
    pub unsafe fn write_cuda(&mut self, device_ptr: *const core::ffi::c_void, byte_len: usize, stream: Option<CudaStream>) -> Result<WriteStatus> {
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
            // SAFETY: The caller upholds the source/context/stream contract and
            // byte_len was checked against the backend's exact frame length.
            unsafe { self.backend.write_cuda_frame(device_ptr, byte_len, stream) }
        }
    }

    /// Closes all resources owned by this instance.
    ///
    /// Closing an already closed camera succeeds. No CUDA API is called because
    /// the camera owns no CUDA allocation, registration, context, or stream.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        #[cfg(windows)]
        let backend_result = self.backend.close();
        #[cfg(not(windows))]
        let backend_result: Result<()> = Ok(());

        trace_debug!(camera_type = ?self.camera_type, "virtual camera closed");
        backend_result
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
