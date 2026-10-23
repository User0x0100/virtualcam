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
