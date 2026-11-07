//! In-process tests of the Media Foundation source contract.

use core::ffi::c_void;

use virtualcam_protocol::mf::{ATTR_CAMERA_ID, ATTR_NATIVE_FORMAT, ATTR_PROTOCOL_VERSION, PROTOCOL_VERSION, SOURCE_CLSID};
use virtualcam_protocol::{FrameRate, PixelFormat, VideoFormat};
use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFGetService, IMFMediaEventGenerator, IMFMediaSource, IMFMediaSourceEx, IMFMediaStream2, IMFSample, MEMediaSample, MENewStream, MESourceStarted, MESourceStopped, MEStreamStarted, MEStreamStopped,
    MEUpdatedStream, MF_E_INVALID_STATE_TRANSITION, MF_E_INVALIDREQUEST, MF_E_MEDIA_SOURCE_WRONGSTATE, MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE, MF_E_UNSUPPORTED_TIME_FORMAT, MF_EVENT_FLAG_NO_WAIT, MF_MT_FRAME_SIZE, MF_MT_SUBTYPE,
    MF_STREAM_STATE_RUNNING, MF_VERSION, MFMEDIASOURCE_IS_LIVE, MFSTARTUP_LITE, MFSampleExtension_Token, MFStartup,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, IClassFactory};
use windows_core::{GUID, HSTRING, IUnknown, Interface};

use super::*;
use crate::stream::variant_i64;

fn init() {
    // SAFETY: Test threads join the MTA and initialize MF (idempotent).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE).expect("MFStartup");
    }
}

fn key(id: virtualcam_protocol::CameraId) -> GUID {
    GUID::from_u128(id.to_u128())
}

fn activate() -> IMFActivate {
    let mut factory: *mut c_void = core::ptr::null_mut();
    let clsid = GUID::from_u128(SOURCE_CLSID.to_u128());
    // SAFETY: Valid pointers for the export.
    assert_eq!(unsafe { DllGetClassObject(&clsid, &IClassFactory::IID, &mut factory) }, S_OK);
    // SAFETY: The export returned one owned factory reference.
    let factory = unsafe { IClassFactory::from_raw(factory) };
    // SAFETY: Valid factory.
    unsafe { factory.CreateInstance(None) }.expect("activate")
}

fn configured(format: &VideoFormat) -> IMFActivate {
    let activate = activate();
    // SAFETY: Plain attribute writes.
    unsafe {
        activate.SetUINT32(&key(ATTR_PROTOCOL_VERSION), PROTOCOL_VERSION).expect("version");
        activate.SetString(&key(ATTR_CAMERA_ID), &HSTRING::from("{11111111-2222-4333-8444-555555555555}")).expect("id");
        activate.SetBlob(&key(ATTR_NATIVE_FORMAT), &format.to_wire().expect("wire").to_bytes()).expect("format");
    }
    activate
}

fn source(activate: &IMFActivate) -> windows_core::Result<IMFMediaSourceEx> {
    // SAFETY: Valid activate.
    unsafe { activate.ActivateObject::<IMFMediaSourceEx>() }
}

/// Drains every queued event type without blocking.
fn events(generator: &IMFMediaEventGenerator) -> Vec<(u32, Option<IUnknown>)> {
    let mut out = Vec::new();
    // SAFETY: Non-blocking reads from a live generator.
    while let Ok(event) = unsafe { generator.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
        // SAFETY: Valid event.
        let kind = unsafe { event.GetType() }.expect("type");
        // SAFETY: Valid event.
        let value = unsafe { event.GetValue() }.unwrap_or_default();
        // SAFETY: VT_UNKNOWN events carry an IUnknown in `punkVal`.
        let unknown = unsafe {
            let inner = &value.Anonymous.Anonymous;
            (inner.vt == windows::Win32::System::Variant::VT_UNKNOWN).then(|| (*inner.Anonymous.punkVal).clone()).flatten()
        };
        out.push((kind, unknown));
    }
    out
}

/// Waits (up to a second) for the next delivered sample; requests that are
/// not due yet are answered by the pacing worker.
fn next_sample(generator: &IMFMediaEventGenerator) -> IMFSample {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if let Some(sample) = events(generator).into_iter().find(|(kind, _)| *kind as i32 == MEMediaSample.0).and_then(|(_, unk)| unk) {
            return sample.cast().expect("IMFSample");
        }
        assert!(std::time::Instant::now() < deadline, "no sample delivered");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

fn kinds(events: &[(u32, Option<IUnknown>)]) -> Vec<i32> {
    events.iter().map(|(kind, _)| *kind as i32).collect()
}

#[test]
fn activation_fails_closed_on_missing_or_malformed_attributes() {
    init();
    let format = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
    assert_eq!(source(&activate()).map(|_| ()).map_err(|e| e.code()), Err(MF_E_INVALIDREQUEST), "no attributes");

    let bad_version = configured(&format);
    // SAFETY: Attribute write.
    unsafe { bad_version.SetUINT32(&key(ATTR_PROTOCOL_VERSION), 99) }.expect("set");
    assert!(source(&bad_version).is_err());

    let oversized = configured(&format);
    // SAFETY: Attribute write.
    unsafe { oversized.SetString(&key(ATTR_CAMERA_ID), &HSTRING::from("x".repeat(10_000))) }.expect("set");
    assert!(source(&oversized).is_err());

    let bad_format = configured(&format);
    // SAFETY: Attribute write with a blob one byte too long.
    unsafe { bad_format.SetBlob(&key(ATTR_NATIVE_FORMAT), &[0_u8; 33]) }.expect("set");
    assert!(source(&bad_format).is_err());

    let odd = configured(&format);
    let wire = VideoFormat::new(PixelFormat::Nv12, 63, 48, FrameRate::fps(30)).to_wire().expect("wire");
    // SAFETY: Attribute write.
    unsafe { odd.SetBlob(&key(ATTR_NATIVE_FORMAT), &wire.to_bytes()) }.expect("set");
    assert!(source(&odd).is_err(), "odd NV12 width is not a valid native mode");
}

#[test]
fn source_follows_the_frame_server_contract() {
    init();
    let format = VideoFormat::new(PixelFormat::Yuy2, 64, 48, FrameRate::fps(30));
    let activate = configured(&format);
    let source = source(&activate).expect("source");
    // Repeated activation returns the same source object.
    let again: IUnknown = unsafe { activate.ActivateObject() }.expect("again");
    assert_eq!(again.as_raw(), source.cast::<IUnknown>().expect("unknown").as_raw());

    // SAFETY: Valid source for the rest of the test.
    unsafe {
        assert_eq!(source.GetCharacteristics().expect("characteristics"), MFMEDIASOURCE_IS_LIVE.0 as u32);
        let a = source.GetSourceAttributes().expect("attrs");
        let b = source.GetSourceAttributes().expect("attrs");
        assert_eq!(a.as_raw(), b.as_raw(), "same source attribute store");
        let s1 = source.GetStreamAttributes(0).expect("stream attrs");
        assert_eq!(s1.as_raw(), source.GetStreamAttributes(0).expect("stream attrs").as_raw(), "same stream attribute store");
        assert_eq!(s1.GetUINT32(&windows::Win32::Media::MediaFoundation::MF_DEVICESTREAM_FRAMESERVER_SHARED).expect("shared"), 1);
        assert!(source.GetStreamAttributes(1).is_err());

        let pd = source.CreatePresentationDescriptor().expect("pd");
        let mut selected = windows_core::BOOL::default();
        let mut sd = None;
        pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd).expect("sd");
        assert!(selected.as_bool());
        let handler = sd.expect("sd").GetMediaTypeHandler().expect("handler");
        assert_eq!(handler.GetMediaTypeCount().expect("count"), 1, "exactly the native mode");
        let mt = handler.GetCurrentMediaType().expect("current");
        assert_eq!(mt.GetGUID(&MF_MT_SUBTYPE).expect("subtype"), GUID::from_u128(PixelFormat::Yuy2.mf_subtype().expect("subtype")));
        assert_eq!(mt.GetUINT64(&MF_MT_FRAME_SIZE).expect("size"), 64 << 32 | 48);

        // Start-position / time-format validation; Pause is unsupported.
        assert_eq!(source.Start(&pd, &GUID::from_u128(1), core::ptr::null()).map_err(|e| e.code()), Err(MF_E_UNSUPPORTED_TIME_FORMAT));
        assert_eq!(source.Start(&pd, core::ptr::null(), &variant_i64(5)).map_err(|e| e.code()), Err(MF_E_INVALIDREQUEST));
        assert_eq!(source.Pause().map_err(|e| e.code()), Err(MF_E_INVALID_STATE_TRANSITION));

        let generator: IMFMediaEventGenerator = source.cast().expect("generator");
        source.Start(&pd, core::ptr::null(), &variant_i64(0)).expect("start");
        let started = events(&generator);
        assert!(kinds(&started).contains(&MENewStream.0) && kinds(&started).contains(&MESourceStarted.0), "{:?}", kinds(&started));
        let stream: IMFMediaStream2 = started
            .iter()
            .find(|(kind, _)| *kind as i32 == MENewStream.0)
            .and_then(|(_, unk)| unk.clone())
            .expect("stream")
            .cast()
            .expect("IMFMediaStream2");
        let stream_events: IMFMediaEventGenerator = stream.cast().expect("generator");
        assert_eq!(kinds(&events(&stream_events)), vec![MEStreamStarted.0]);
        assert_eq!(stream.GetStreamState().expect("state"), MF_STREAM_STATE_RUNNING);
        assert_eq!(stream.GetMediaSource().expect("parent").as_raw(), source.cast::<IMFMediaSource>().expect("source").as_raw());

        // Synchronous RequestSample: token echoed, black with no producer.
        let token: IUnknown = source.GetSourceAttributes().expect("any object").cast().expect("unknown");
        stream.RequestSample(&token).expect("request");
        let delivered = events(&stream_events);
        assert_eq!(kinds(&delivered), vec![MEMediaSample.0]);
        let sample: IMFSample = delivered[0].1.clone().expect("sample").cast().expect("IMFSample");
        let echoed: IUnknown = sample.GetUnknown(&MFSampleExtension_Token).expect("token");
        assert_eq!(echoed.as_raw(), token.as_raw());
        assert!(sample.GetSampleTime().expect("time") > 0);
        assert_eq!(sample.GetSampleDuration().expect("duration"), 333_333);
        let buffer = sample.ConvertToContiguousBuffer().expect("buffer");
        let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
        buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
        let bytes = core::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock().expect("unlock");
        assert_eq!(bytes.len(), 64 * 48 * 2);
        assert!(bytes.chunks_exact(2).all(|pair| pair == [16, 128]), "limited-range YUY2 black");

        // Stopped: wrong state; restart announces an updated stream.
        source.Stop().expect("stop");
        assert!(kinds(&events(&generator)).contains(&MESourceStopped.0));
        assert_eq!(kinds(&events(&stream_events)), vec![MEStreamStopped.0]);
        assert_eq!(stream.RequestSample(None).map_err(|e| e.code()), Err(MF_E_MEDIA_SOURCE_WRONGSTATE));
        source.Start(&pd, core::ptr::null(), core::ptr::null()).expect("restart");
        assert!(kinds(&events(&generator)).contains(&MEUpdatedStream.0));

        // Unsupported services/controls use their contract HRESULTs.
        assert_eq!(
            source.cast::<IMFGetService>().expect("service").GetService::<IUnknown>(&GUID::zeroed()).map(|_| ()).map_err(|e| e.code()),
            Err(MF_E_UNSUPPORTED_SERVICE)
        );
        let ks: IKsControl = source.cast().expect("IKsControl");
        let id = KSIDENTIFIER::default();
        let mut returned = 0;
        assert_eq!(
            ks.KsProperty(&id, core::mem::size_of::<KSIDENTIFIER>() as u32, core::ptr::null_mut(), 0, &mut returned).map_err(|e| e.code()),
            Err(ERROR_SET_NOT_FOUND.to_hresult())
        );

        // Shutdown: every public method reports MF_E_SHUTDOWN.
        source.Shutdown().expect("shutdown");
        assert_eq!(source.Start(&pd, core::ptr::null(), core::ptr::null()).map_err(|e| e.code()), Err(MF_E_SHUTDOWN));
        assert_eq!(source.GetCharacteristics().map_err(|e| e.code()), Err(MF_E_SHUTDOWN));
        assert_eq!(source.GetSourceAttributes().map(|_| ()).map_err(|e| e.code()), Err(MF_E_SHUTDOWN));
        assert_eq!(stream.RequestSample(None).map_err(|e| e.code()), Err(MF_E_SHUTDOWN));
        assert_eq!(stream.GetMediaSource().map(|_| ()).map_err(|e| e.code()), Err(MF_E_SHUTDOWN));
        assert_eq!(source.Shutdown().map_err(|e| e.code()), Err(MF_E_SHUTDOWN));

        // A shut-down cached source is replaced on the next activation.
        let fresh = activate.ActivateObject::<IMFMediaSourceEx>().expect("fresh source");
        assert_ne!(fresh.as_raw(), source.as_raw());
        fresh.Shutdown().expect("shutdown fresh");
    }
}

#[test]
fn attributes_forward_to_the_activation_store() {
    init();
    let activate = activate();
    let attributes: IMFAttributes = activate.cast().expect("IMFAttributes");
    // SAFETY: Plain attribute calls.
    unsafe {
        attributes.SetUINT64(&GUID::from_u128(7), 42).expect("set");
        assert_eq!(activate.GetUINT64(&GUID::from_u128(7)).expect("get"), 42);
        assert_eq!(activate.GetCount().expect("count"), 1);
        activate.DeleteAllItems().expect("clear");
        assert_eq!(activate.GetCount().expect("count"), 0);
        assert_eq!(activate.ShutdownObject().map_err(|e| e.code()), Err(windows::Win32::Foundation::E_NOTIMPL));
        assert_eq!(activate.DetachObject().map_err(|e| e.code()), Err(windows::Win32::Foundation::E_NOTIMPL));
    }
}

/// Raw layout of a delivered sample's buffer: the pitch its memory really
/// uses (a 2D buffer's own pitch, or the media type's stride for a linear one).
fn raw_pitch(sample: &IMFSample) -> i32 {
    use windows::Win32::Media::MediaFoundation::IMF2DBuffer;
    // SAFETY: Valid sample with one buffer; the lock is balanced.
    unsafe {
        let buffer = sample.GetBufferByIndex(0).expect("buffer");
        let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() else { return 0 };
        let (mut scanline0, mut pitch) = (core::ptr::null_mut(), 0_i32);
        buffer2d.Lock2D(&mut scanline0, &mut pitch).expect("lock2d");
        buffer2d.Unlock2D().expect("unlock2d");
        pitch
    }
}

/// Frame Server and the DirectShow bridge read a sample as a tight frame of
/// `MF_MT_DEFAULT_STRIDE`; a 2D buffer whose rows are padded for alignment
/// (NV12 1080 wide -> 1088) is sheared on that path. Every delivered buffer
/// must therefore be tight, with or without a pipeline allocator.
#[test]
fn delivered_buffers_are_tight_for_unaligned_widths() {
    use windows::Win32::Media::MediaFoundation::{IMFSampleAllocatorControl, IMFVideoSampleAllocator, MFCreateVideoSampleAllocatorEx};
    init();
    let format = VideoFormat::new(PixelFormat::Nv12, 1080, 8, FrameRate::fps(30));
    for with_allocator in [false, true] {
        let activate = configured(&format);
        let source = source(&activate).expect("source");
        // SAFETY: Valid source for the rest of the iteration.
        unsafe {
            if with_allocator {
                let mut raw: *mut c_void = core::ptr::null_mut();
                MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocator::IID, &mut raw).expect("allocator");
                let allocator = IMFVideoSampleAllocator::from_raw(raw);
                source.cast::<IMFSampleAllocatorControl>().expect("allocator control").SetDefaultAllocator(0, &allocator).expect("set allocator");
            }
            let pd = source.CreatePresentationDescriptor().expect("pd");
            let generator: IMFMediaEventGenerator = source.cast().expect("generator");
            source.Start(&pd, core::ptr::null(), &variant_i64(0)).expect("start");
            let stream: IMFMediaStream2 = events(&generator)
                .into_iter()
                .find(|(kind, _)| *kind as i32 == MENewStream.0)
                .and_then(|(_, unk)| unk)
                .expect("stream")
                .cast()
                .expect("IMFMediaStream2");
            let stream_events: IMFMediaEventGenerator = stream.cast().expect("generator");
            let _ = events(&stream_events);
            for _ in 0..2 {
                stream.RequestSample(None).expect("request");
                let sample = next_sample(&stream_events);
                let pitch = raw_pitch(&sample);
                assert!(pitch == 0 || pitch == 1080, "allocator={with_allocator}: delivered pitch {pitch}, expected a tight 1080");
                // The raw (not converted) memory holds limited-range black.
                let buffer = sample.GetBufferByIndex(0).expect("buffer");
                let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
                buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
                let bytes = core::slice::from_raw_parts(data, length as usize).to_vec();
                buffer.Unlock().expect("unlock");
                assert_eq!(bytes.len(), 1080 * 8 * 3 / 2);
                assert!(bytes[..1080 * 8].iter().all(|&y| y == 16) && bytes[1080 * 8..].iter().all(|&c| c == 128), "allocator={with_allocator}: black frame");
            }
            source.Shutdown().expect("shutdown");
        }
    }
}

/// An NV12 camera also offers YUY2, converted by the source itself, so the
/// DirectShow bridge never converts; the selected type takes effect at start
/// and can change across restarts.
#[test]
fn consumers_select_a_converted_mode() {
    use windows::Win32::Media::MediaFoundation::{IMFSampleAllocatorControl, IMFVideoSampleAllocator, MFCreateVideoSampleAllocatorEx};
    init();
    let format = VideoFormat::new(PixelFormat::Nv12, 1080, 8, FrameRate::fps(30));
    let subtype = |format: PixelFormat| GUID::from_u128(format.mf_subtype().expect("subtype"));
    for with_allocator in [false, true] {
        let source = source(&configured(&format)).expect("source");
        // SAFETY: Valid source for the rest of the iteration.
        unsafe {
            if with_allocator {
                let mut raw: *mut c_void = core::ptr::null_mut();
                MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocator::IID, &mut raw).expect("allocator");
                source.cast::<IMFSampleAllocatorControl>().expect("control").SetDefaultAllocator(0, &IMFVideoSampleAllocator::from_raw(raw)).expect("set");
            }
            let pd = source.CreatePresentationDescriptor().expect("pd");
            let mut selected = windows_core::BOOL::default();
            let mut sd = None;
            pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd).expect("sd");
            let handler = sd.expect("sd").GetMediaTypeHandler().expect("handler");
            let offered: Vec<GUID> = (0..handler.GetMediaTypeCount().expect("count"))
                .map(|i| handler.GetMediaTypeByIndex(i).expect("type").GetGUID(&MF_MT_SUBTYPE).expect("subtype"))
                .collect();
            assert_eq!(
                offered,
                [subtype(PixelFormat::Yuy2), subtype(PixelFormat::Nv12)],
                "conversions first (the DirectShow bridge takes the first type it can convert from), native last"
            );
            assert_eq!(handler.GetCurrentMediaType().expect("current").GetGUID(&MF_MT_SUBTYPE).expect("subtype"), subtype(PixelFormat::Nv12));
            let yuy2 = handler.GetMediaTypeByIndex(0).expect("yuy2");
            assert_eq!(yuy2.GetUINT64(&MF_MT_FRAME_SIZE).expect("size"), 1080 << 32 | 8);
            assert_eq!(yuy2.GetUINT32(&windows::Win32::Media::MediaFoundation::MF_MT_DEFAULT_STRIDE).expect("stride"), 2160);

            let generator: IMFMediaEventGenerator = source.cast().expect("generator");
            let mut stream: Option<IMFMediaStream2> = None;
            for (pick, pitch_bytes, bytes, black) in [(0, 2160, 1080 * 8 * 2, [16_u8, 128]), (1, 1080, 1080 * 8 * 3 / 2, [16, 16])] {
                handler.SetCurrentMediaType(&handler.GetMediaTypeByIndex(pick).expect("type")).expect("select");
                source.Start(&pd, core::ptr::null(), &variant_i64(0)).expect("start");
                let started = events(&generator);
                if stream.is_none() {
                    stream = started
                        .iter()
                        .find(|(kind, _)| *kind as i32 == MENewStream.0)
                        .and_then(|(_, unk)| unk.clone())
                        .map(|unk| unk.cast().expect("IMFMediaStream2"));
                }
                let stream = stream.as_ref().expect("stream");
                let stream_events: IMFMediaEventGenerator = stream.cast().expect("generator");
                let _ = events(&stream_events);
                stream.RequestSample(None).expect("request");
                let sample = next_sample(&stream_events);
                let pitch = raw_pitch(&sample);
                assert!(pitch == 0 || pitch == pitch_bytes, "allocator={with_allocator} pick={pick}: pitch {pitch}");
                let buffer = sample.GetBufferByIndex(0).expect("buffer");
                let (mut data, mut length) = (core::ptr::null_mut(), 0_u32);
                buffer.Lock(&mut data, None, Some(&mut length)).expect("lock");
                let frame = core::slice::from_raw_parts(data, length as usize).to_vec();
                buffer.Unlock().expect("unlock");
                assert_eq!(frame.len(), bytes, "allocator={with_allocator} pick={pick}");
                assert_eq!(&frame[..2], &black, "allocator={with_allocator} pick={pick}: black");
                source.Stop().expect("stop");
                let _ = events(&generator);
            }
            source.Shutdown().expect("shutdown");
        }
    }
}

/// Consumers ask for the next sample as soon as one arrives; without pacing
/// that spins at thousands of frames per second. Without a producer, black
/// samples are delivered at the native frame interval, the first at once.
#[test]
fn requests_are_answered_at_the_frame_rate() {
    init();
    let source = source(&configured(&VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(50)))).expect("source");
    // SAFETY: Valid source for the rest of the test.
    unsafe {
        let pd = source.CreatePresentationDescriptor().expect("pd");
        let generator: IMFMediaEventGenerator = source.cast().expect("generator");
        source.Start(&pd, core::ptr::null(), &variant_i64(0)).expect("start");
        let stream: IMFMediaStream2 = events(&generator)
            .into_iter()
            .find(|(kind, _)| *kind as i32 == MENewStream.0)
            .and_then(|(_, unk)| unk)
            .expect("stream")
            .cast()
            .expect("IMFMediaStream2");
        let stream_events: IMFMediaEventGenerator = stream.cast().expect("generator");
        let _ = events(&stream_events);
        let started = std::time::Instant::now();
        stream.RequestSample(None).expect("request");
        assert_eq!(kinds(&events(&stream_events)), vec![MEMediaSample.0], "the first request is answered at once");
        // A consumer that asks again right away gets one sample per 20 ms.
        for _ in 0..10 {
            stream.RequestSample(None).expect("request");
            let _ = next_sample(&stream_events);
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(195), "11 samples at 50 FPS took {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_millis(400), "pacing keeps up: {elapsed:?}");
        // Stopping drops waiting requests; nothing arrives afterwards.
        stream.RequestSample(None).expect("request");
        source.Stop().expect("stop");
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(!kinds(&events(&stream_events)).contains(&MEMediaSample.0), "no sample after stop");
        source.Shutdown().expect("shutdown");
    }
}
