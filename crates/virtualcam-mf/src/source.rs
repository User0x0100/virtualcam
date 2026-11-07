//! The media source: live and request-driven. It offers the conversions it
//! performs itself, then the immutable native mode the producer writes
//! (`virtualcam_protocol::convert`), so consumers never depend on Windows'
//! own conversion in Frame Server's DirectShow bridge.
//!
//! Lock hierarchy is Source -> Stream only: source methods may call stream
//! methods while holding the source lock; stream methods never call back
//! into the source. `Shutdown` breaks the Source <-> Stream reference cycle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use virtualcam_protocol::convert::conversions_from;
use virtualcam_protocol::{ValidatedFormat, VideoFormat};
use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, ERROR_SET_NOT_FOUND, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFGetService, IMFGetService_Impl, IMFMediaEvent, IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource_Impl, IMFMediaSourceEx, IMFMediaSourceEx_Impl, IMFMediaType,
    IMFPresentationDescriptor, IMFSampleAllocatorControl, IMFSampleAllocatorControl_Impl, IMFVideoSampleAllocator, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MENewStream, MESourceStarted, MESourceStopped, MEUpdatedStream,
    MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MF_DEVICESTREAM_FRAMESERVER_SHARED, MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID, MF_E_INVALID_STATE_TRANSITION, MF_E_INVALIDREQUEST, MF_E_INVALIDSTREAMNUMBER,
    MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE, MF_E_UNSUPPORTED_TIME_FORMAT, MF_MT_ALL_SAMPLES_INDEPENDENT, MF_MT_DEFAULT_STRIDE, MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MFCreateAttributes, MFCreateEventQueue, MFCreateMediaType,
    MFCreatePresentationDescriptor, MFCreateStreamDescriptor, MFFrameSourceTypes_Color, MFGetSystemTime, MFMEDIASOURCE_IS_LIVE, MFMediaType_Video, MFSampleAllocatorUsage, MFSampleAllocatorUsage_UsesProvidedAllocator,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::Marshal::IMarshal;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Variant::{VT_EMPTY, VT_I8};
use windows_core::{ComObject, GUID, HRESULT, IUnknown, Interface, Ref, implement};

use crate::config::SourceConfig;
use crate::ftm::{Ftm, forward_marshal};
use crate::rendezvous::{Rendezvous, TransportSlot};
use crate::stream::{MediaStream, variant_i64};
use crate::{ModuleRef, guard};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Stopped,
    Started,
    Shutdown,
}

struct SourceInner {
    state: State,
    stream: Option<ComObject<MediaStream>>,
    /// `MENewStream` was sent; later starts send `MEUpdatedStream`.
    announced: bool,
    rendezvous: Option<Rendezvous>,
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl, IMarshal, IAgileObject)]
pub(crate) struct MediaSource {
    queue: IMFMediaEventQueue,
    attributes: IMFAttributes,
    presentation: IMFPresentationDescriptor,
    inner: Mutex<SourceInner>,
    shut_down: AtomicBool,
    ftm: Ftm,
    _module: ModuleRef,
}

/// The offered modes: `native` first, then each conversion from it with the
/// same size, rate and colorimetry.
pub(crate) fn offered_formats(native: &ValidatedFormat) -> Vec<ValidatedFormat> {
    let base = native.format();
    let converted = conversions_from(base.pixel_format).iter().filter_map(|&pixel_format| VideoFormat { pixel_format, ..base }.validate().ok());
    core::iter::once(*native).chain(converted).collect()
}

/// Builds the media type of one offered mode.
pub(crate) fn mode_media_type(format: &ValidatedFormat) -> windows_core::Result<IMFMediaType> {
    let video = format.format();
    let subtype = video.pixel_format.mf_subtype().ok_or_else(|| windows_core::Error::from(MF_E_INVALIDREQUEST))?;
    // SAFETY: Plain MF object creation and attribute writes.
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &GUID::from_u128(subtype))?;
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, u64::from(video.width) << 32 | u64::from(video.height))?;
        media_type.SetUINT64(&MF_MT_FRAME_RATE, u64::from(video.frame_rate.numerator) << 32 | u64::from(video.frame_rate.denominator))?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        media_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        media_type.SetUINT32(&MF_MT_SAMPLE_SIZE, format.frame_size() as u32)?;
        // Top-down for every mode, RGB included: a negative (bottom-up) RGB
        // stride makes Frame Server's DirectShow bridge crash FFmpeg's
        // DirectShow input (measured 2026-11-07).
        media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, format.layout().planes()[0].pitch as u32)?;
        let color = video.color;
        if video.pixel_format.is_yuv() && color.matrix.code() != 0 {
            media_type.SetUINT32(&MF_MT_YUV_MATRIX, u32::from(color.matrix.code()))?;
        }
        for (key, code) in [
            (&MF_MT_VIDEO_NOMINAL_RANGE, color.range.code()),
            (&MF_MT_VIDEO_PRIMARIES, color.primaries.code()),
            (&MF_MT_TRANSFER_FUNCTION, color.transfer.code()),
        ] {
            if code != 0 {
                media_type.SetUINT32(key, u32::from(code))?;
            }
        }
        Ok(media_type)
    }
}

/// Frame Server stream attributes, set on both the stream store and the
/// stream descriptor.
fn set_stream_attributes(target: &IMFAttributes) -> windows_core::Result<()> {
    // SAFETY: Plain attribute writes.
    unsafe {
        target.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        target.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        target.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        target.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)
    }
}

impl MediaSource {
    pub(crate) fn create(config: SourceConfig) -> windows_core::Result<ComObject<Self>> {
        let offers = offered_formats(&config.format)
            .into_iter()
            .map(|format| mode_media_type(&format).map(|media_type| (format, media_type)))
            .collect::<windows_core::Result<Vec<_>>>()?;
        // Converted modes are listed before the native one. Frame Server's
        // DirectShow bridge serves a DirectShow request from the first listed
        // type it can convert from, so with NV12 first it answered YUY2
        // requests with its own (shearing) conversion even though our YUY2
        // was offered; with YUY2 first it passes every listed type through.
        let types: Vec<Option<IMFMediaType>> = offers[1..].iter().chain(&offers[..1]).map(|(_, media_type)| Some(media_type.clone())).collect();
        // SAFETY: Plain MF object creation.
        let (descriptor, presentation, source_queue, stream_queue) = unsafe {
            let descriptor = MFCreateStreamDescriptor(0, &types)?;
            // The native mode is current until a consumer selects another.
            descriptor.GetMediaTypeHandler()?.SetCurrentMediaType(&offers[0].1)?;
            let presentation = MFCreatePresentationDescriptor(Some(&[Some(descriptor.clone())]))?;
            presentation.SelectStream(0)?;
            (descriptor, presentation, MFCreateEventQueue()?, MFCreateEventQueue()?)
        };
        let attributes = |count| -> windows_core::Result<IMFAttributes> {
            let mut store = None;
            // SAFETY: Out pointer receives a new store.
            unsafe { MFCreateAttributes(&mut store, count) }?;
            store.ok_or_else(|| windows::Win32::Foundation::E_OUTOFMEMORY.into())
        };
        let stream_attributes = attributes(4)?;
        set_stream_attributes(&stream_attributes)?;
        set_stream_attributes(&descriptor.cast()?)?;

        let transport = Arc::new(TransportSlot::new());
        let stream = ComObject::new(MediaStream::new(stream_queue, descriptor, stream_attributes, offers, Arc::clone(&transport)));
        let rendezvous = Rendezvous::spawn(&config, transport);
        let source = ComObject::new(Self {
            queue: source_queue,
            attributes: attributes(1)?,
            presentation,
            inner: Mutex::new(SourceInner {
                state: State::Stopped,
                stream: Some(stream.clone()),
                announced: false,
                rendezvous,
            }),
            shut_down: AtomicBool::new(false),
            ftm: Ftm::default(),
            _module: ModuleRef::new(),
        });
        source.ftm.attach(&source.to_interface::<IUnknown>())?;
        stream.set_source(source.to_interface::<IMFMediaSourceEx>().into());
        Ok(source)
    }

    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::SeqCst)
    }

    fn inner(&self) -> MutexGuard<'_, SourceInner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn live(&self) -> windows_core::Result<MutexGuard<'_, SourceInner>> {
        let inner = self.inner();
        if inner.state == State::Shutdown { Err(MF_E_SHUTDOWN.into()) } else { Ok(inner) }
    }

    fn queue_source_event(&self, event: i32, value: &PROPVARIANT) -> windows_core::Result<()> {
        // SAFETY: Valid queue and variant.
        unsafe { self.queue.QueueEventParamVar(event as u32, &GUID::zeroed(), S_OK, value) }
    }
}

forward_marshal!(MediaSource_Impl);

impl IAgileObject_Impl for MediaSource_Impl {}

/// Live sources accept only "now": `VT_EMPTY`, or `VT_I8` equal to zero.
///
/// # Safety
///
/// `position` must be null or a valid PROPVARIANT.
unsafe fn validate_start_position(position: *const PROPVARIANT) -> windows_core::Result<()> {
    // SAFETY: Caller contract.
    let Some(position) = (unsafe { position.as_ref() }) else { return Ok(()) };
    // SAFETY: Reading the discriminant and, for VT_I8, its union member.
    let (vt, value) = unsafe { (position.Anonymous.Anonymous.vt, position.Anonymous.Anonymous.Anonymous.hVal) };
    match vt {
        VT_EMPTY => Ok(()),
        VT_I8 if value == 0 => Ok(()),
        _ => Err(MF_E_INVALIDREQUEST.into()),
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows_core::Result<IMFMediaEvent> {
        // GetEvent may block forever: check shutdown, release the lock, wait.
        drop(self.live()?);
        // SAFETY: Valid queue.
        unsafe { self.queue.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows_core::Result<()> {
        drop(self.live()?);
        // SAFETY: Valid queue and forwarded references.
        unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows_core::Result<IMFMediaEvent> {
        drop(self.live()?);
        // SAFETY: Valid queue and forwarded reference.
        unsafe { self.queue.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(&self, met: u32, extended: *const GUID, status: HRESULT, value: *const PROPVARIANT) -> windows_core::Result<()> {
        drop(self.live()?);
        // SAFETY: Forwarded arguments.
        unsafe { self.queue.QueueEventParamVar(met, extended, status, value) }
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> windows_core::Result<u32> {
        drop(self.live()?);
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows_core::Result<IMFPresentationDescriptor> {
        drop(self.live()?);
        // SAFETY: Valid template descriptor.
        unsafe { self.presentation.Clone() }
    }

    fn Start(&self, presentation: Ref<IMFPresentationDescriptor>, time_format: *const GUID, position: *const PROPVARIANT) -> windows_core::Result<()> {
        guard(|| {
            let mut inner = self.live()?;
            let presentation = presentation.ok().map_err(|_| windows_core::Error::from(E_POINTER))?;
            // SAFETY: COM passes null or a valid GUID.
            if unsafe { time_format.as_ref() }.is_some_and(|format| *format != GUID::zeroed()) {
                return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
            }
            // SAFETY: COM passes null or a valid PROPVARIANT.
            unsafe { validate_start_position(position) }?;
            let mut selected = windows_core::BOOL::default();
            let mut descriptor = None;
            // SAFETY: Valid descriptor; outputs writable.
            unsafe { presentation.GetStreamDescriptorByIndex(0, &mut selected, &mut descriptor) }?;
            let stream = inner.stream.clone().ok_or_else(|| windows_core::Error::from(MF_E_SHUTDOWN))?;
            // SAFETY: Plain clock read.
            let now = unsafe { MFGetSystemTime() };
            if selected.as_bool() {
                let descriptor = descriptor.ok_or_else(|| windows_core::Error::from(E_POINTER))?;
                // SAFETY: Valid descriptor.
                let current = unsafe { descriptor.GetMediaTypeHandler()?.GetCurrentMediaType() }?;
                let output = stream.offer_of(&current)?;
                let event = if inner.announced { MEUpdatedStream } else { MENewStream };
                let unknown: IUnknown = stream.to_interface();
                // SAFETY: Valid queue and stream.
                unsafe { self.queue.QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &unknown) }?;
                inner.announced = true;
                stream.start(now, output)?;
            } else if stream.is_running() {
                stream.stop()?;
            }
            inner.state = State::Started;
            self.queue_source_event(MESourceStarted.0, &variant_i64(now))
        })
    }

    fn Stop(&self) -> windows_core::Result<()> {
        guard(|| {
            let mut inner = self.live()?;
            if let Some(stream) = &inner.stream {
                stream.stop()?;
            }
            inner.state = State::Stopped;
            self.queue_source_event(MESourceStopped.0, &PROPVARIANT::default())
        })
    }

    fn Pause(&self) -> windows_core::Result<()> {
        drop(self.live()?);
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> windows_core::Result<()> {
        guard(|| {
            let (stream, rendezvous) = {
                let mut inner = self.live()?;
                inner.state = State::Shutdown;
                self.shut_down.store(true, Ordering::SeqCst);
                // SAFETY: Valid queue; later calls report MF_E_SHUTDOWN.
                let _ = unsafe { self.queue.Shutdown() };
                (inner.stream.take(), inner.rendezvous.take())
            };
            // Outside the source lock: shut the stream down (breaking its
            // strong reference back to us), then cancel and join the worker.
            if let Some(stream) = stream {
                stream.shutdown();
            }
            if let Some(rendezvous) = rendezvous {
                rendezvous.stop();
            }
            Ok(())
        })
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> windows_core::Result<IMFAttributes> {
        drop(self.live()?);
        // The same store every time, never a copy.
        Ok(self.attributes.clone())
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> windows_core::Result<IMFAttributes> {
        let inner = self.live()?;
        match (&inner.stream, stream_id) {
            (Some(stream), 0) => Ok(stream.attributes.clone()),
            _ => Err(MF_E_INVALIDSTREAMNUMBER.into()),
        }
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> windows_core::Result<()> {
        drop(self.live()?);
        // CPU-only source.
        Err(E_NOTIMPL.into())
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(&self, _service: *const GUID, _riid: *const GUID, ppv: *mut *mut core::ffi::c_void) -> windows_core::Result<()> {
        if !ppv.is_null() {
            // SAFETY: Non-null caller out pointer.
            unsafe { *ppv = core::ptr::null_mut() };
        }
        drop(self.live()?);
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(&self, _property: *const KSIDENTIFIER, _length: u32, _data: *mut core::ffi::c_void, _data_length: u32, _returned: *mut u32) -> windows_core::Result<()> {
        drop(self.live()?);
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }

    fn KsMethod(&self, _method: *const KSIDENTIFIER, _length: u32, _data: *mut core::ffi::c_void, _data_length: u32, _returned: *mut u32) -> windows_core::Result<()> {
        drop(self.live()?);
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }

    fn KsEvent(&self, _event: *const KSIDENTIFIER, _length: u32, _data: *mut core::ffi::c_void, _data_length: u32, _returned: *mut u32) -> windows_core::Result<()> {
        drop(self.live()?);
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }
}

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, stream_id: u32, allocator: Ref<IUnknown>) -> windows_core::Result<()> {
        let inner = self.live()?;
        let stream = inner.stream.as_ref().filter(|_| stream_id == 0).ok_or_else(|| windows_core::Error::from(MF_E_INVALIDSTREAMNUMBER))?;
        let allocator = allocator.as_ref().map(Interface::cast::<IMFVideoSampleAllocator>).transpose()?;
        stream.set_allocator(allocator)
    }

    fn GetAllocatorUsage(&self, stream_id: u32, input_stream_id: *mut u32, usage: *mut MFSampleAllocatorUsage) -> windows_core::Result<()> {
        drop(self.live()?);
        if stream_id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        if input_stream_id.is_null() || usage.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: Non-null caller out pointers.
        unsafe {
            *input_stream_id = 0;
            *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
        }
        Ok(())
    }
}
