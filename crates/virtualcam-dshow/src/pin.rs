//! The capture output pin and the pin / media-type enumerators.

use std::sync::{Arc, Mutex};

use virtualcam_protocol::VideoFormat;
use virtualcam_protocol::dshow::{DshowNames, PIN_NAME, guids};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, ERROR_BUSY, S_FALSE, S_OK, SIZE};
use windows::Win32::Media::DirectShow::{
    ALLOCATOR_PROPERTIES, E_PROP_ID_UNSUPPORTED, E_PROP_SET_UNSUPPORTED, IAMStreamConfig, IAMStreamConfig_Impl, IBaseFilter, IEnumMediaTypes, IEnumMediaTypes_Impl, IEnumPins, IEnumPins_Impl, IMemAllocator, IMemInputPin, IPin,
    IPin_Impl, PIN_DIRECTION, PIN_INFO, PINDIR_OUTPUT, State_Stopped, VFW_E_ALREADY_CONNECTED, VFW_E_INVALIDMEDIATYPE, VFW_E_NO_ACCEPTABLE_TYPES, VFW_E_NOT_CONNECTED, VFW_E_NOT_STOPPED, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc};
use windows_core::{GUID, HRESULT, IUnknown, IUnknownImpl, Interface, OutRef, PWSTR, Ref, Weak, implement};

use crate::consumer::{ConsumerLease, LeaseError};
use crate::filter::{Agile, Connection, Shared};
use crate::media_type::{HEADERS, Header, MediaType, Parsed, parse, sample_layout};
use crate::{ModuleRef, guard, guard_hr};

/// `AMPROPERTY_PIN_CATEGORY`.
const AMPROPERTY_PIN_CATEGORY: u32 = 0;
/// `KSPROPERTY_SUPPORT_GET`.
const KSPROPERTY_SUPPORT_GET: u32 = 1;
const MIN_BUFFERS: i32 = 3;

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
pub(crate) struct OutputPin {
    shared: Arc<Shared>,
    filter: Option<Weak<IUnknown>>,
    _module: ModuleRef,
}

fn pin_name() -> [u16; 128] {
    let mut name = [0_u16; 128];
    for (slot, unit) in name.iter_mut().zip(PIN_NAME.encode_utf16()) {
        *slot = unit;
    }
    name
}

fn co_task_string(value: &str) -> windows_core::Result<PWSTR> {
    let units: Vec<u16> = value.encode_utf16().chain(core::iter::once(0)).collect();
    // SAFETY: Allocation released by the caller with CoTaskMemFree.
    let buffer = unsafe { CoTaskMemAlloc(units.len() * 2) }.cast::<u16>();
    if buffer.is_null() {
        return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
    }
    // SAFETY: `buffer` holds `units.len()` writable u16s.
    unsafe { core::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len()) };
    Ok(PWSTR(buffer))
}

impl OutputPin {
    pub(crate) fn create(shared: Arc<Shared>, filter: Option<Weak<IUnknown>>) -> IPin {
        Self { shared, filter, _module: ModuleRef::new() }.into()
    }

    /// The media types offered while unconnected (or the connected one).
    fn offered_types(&self) -> Vec<MediaType> {
        if let Some(connection) = &self.shared.state().connection {
            return vec![connection.media_type.clone()];
        }
        self.shared.offered().iter().flat_map(|format| HEADERS.iter().filter_map(|header| MediaType::new(format, *header))).collect()
    }

    /// Negotiates an allocator with the downstream input pin.
    fn decide_allocator(receiver: &IPin, sample_size: usize) -> windows_core::Result<(IMemInputPin, IMemAllocator)> {
        let input: IMemInputPin = receiver.cast()?;
        let size = i32::try_from(sample_size).map_err(|_| windows_core::Error::from(E_INVALIDARG))?;
        // SAFETY: Valid interface; E_NOTIMPL simply means "no requirements".
        let required = unsafe { input.GetAllocatorRequirements() }.unwrap_or_default();
        let request = ALLOCATOR_PROPERTIES {
            cBuffers: required.cBuffers.max(MIN_BUFFERS),
            cbBuffer: required.cbBuffer.max(size),
            cbAlign: required.cbAlign.max(1),
            cbPrefix: required.cbPrefix.max(0),
        };
        let configure = |allocator: IMemAllocator| -> windows_core::Result<(IMemInputPin, IMemAllocator)> {
            // SAFETY: Valid allocator; `request` outlives the call.
            let actual = unsafe { allocator.SetProperties(&request) }?;
            if actual.cbBuffer < size || actual.cBuffers < 1 {
                return Err(VFW_E_NO_ACCEPTABLE_TYPES.into());
            }
            // SAFETY: Valid interfaces.
            unsafe { input.NotifyAllocator(&allocator, false) }?;
            Ok((input.clone(), allocator))
        };
        // Prefer the downstream allocator (it may hand out video memory or
        // pooled buffers), then fall back to the standard memory allocator.
        // SAFETY: Valid interface.
        if let Ok(allocator) = unsafe { input.GetAllocator() }
            && let Ok(result) = configure(allocator)
        {
            return Ok(result);
        }
        // SAFETY: Standard in-process activation.
        let allocator: IMemAllocator = unsafe { CoCreateInstance(&GUID::from_u128(guids::MEMORY_ALLOCATOR), None, CLSCTX_INPROC_SERVER) }?;
        configure(allocator)
    }
}

impl OutputPin_Impl {
    /// Attempts one media type against `receiver`; on success the lease
    /// moves into the connection.
    fn try_connect(&self, receiver: &IPin, mt: MediaType, offered: &[VideoFormat], lease: &mut Option<ConsumerLease>) -> windows_core::Result<()> {
        // SAFETY: `mt` is one of our valid media types.
        let parsed = unsafe { parse(&mt.0) }.ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?;
        let format = parsed.to_format(base_for(offered, &parsed).ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?);
        let layout = sample_layout(&format).ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?;
        let this: IPin = self.to_interface();
        // SAFETY: Valid interfaces; `mt` outlives the call.
        unsafe { receiver.ReceiveConnection(&this, &mt.0) }?;
        match OutputPin::decide_allocator(receiver, layout.size()) {
            Ok((input, allocator)) => {
                let mut lease = lease.take().ok_or(windows_core::Error::from(E_UNEXPECTED))?;
                lease.publish(&format);
                self.shared.state().connection = Some(Connection {
                    peer: Agile(receiver.clone()),
                    input: Agile(input),
                    allocator: Agile(allocator),
                    media_type: mt,
                    format,
                    layout,
                    flip: !format.pixel_format.is_yuv() && !parsed.top_down,
                    _lease: lease,
                });
                Ok(())
            }
            Err(error) => {
                // SAFETY: Valid interface; undo the accepted connection.
                let _ = unsafe { receiver.Disconnect() };
                Err(error)
            }
        }
    }
}

/// The offered format a consumer type builds on: the one with its pixel
/// format. `None` for pixel formats this camera does not offer.
fn base_for<'a>(offered: &'a [VideoFormat], parsed: &Parsed) -> Option<&'a VideoFormat> {
    offered.iter().find(|format| format.pixel_format == parsed.pixel_format)
}

/// Deep-copies a caller media type when it fully specifies a format we can
/// produce; partially specified types are matched against our own.
fn candidate_types(requested: *const AM_MEDIA_TYPE, ours: Vec<MediaType>, offered: &[VideoFormat]) -> Vec<MediaType> {
    // SAFETY: COM passes null or a valid AM_MEDIA_TYPE.
    let Some(request) = (unsafe { requested.as_ref() }) else { return ours };
    // SAFETY: As above.
    if let Some(parsed) = unsafe { parse(requested) } {
        let header = if request.formattype == GUID::from_u128(guids::FORMAT_VIDEO_INFO2) {
            Header::VideoInfo2
        } else {
            Header::VideoInfo
        };
        let Some(base) = base_for(offered, &parsed) else { return Vec::new() };
        let format = parsed.to_format(base);
        return MediaType::new(&format, header)
            .map(|mut mt| {
                if parsed.top_down {
                    // Honor an explicit top-down RGB request.
                    // SAFETY: Our own block; both header flavors end with the
                    // BITMAPINFOHEADER whose biHeight we negate.
                    unsafe { negate_height(&mut mt) };
                }
                mt
            })
            .into_iter()
            .collect();
    }
    let wildcard = |field: GUID, ours: GUID| field == GUID::zeroed() || field == ours;
    ours.into_iter()
        .filter(|mt| wildcard(request.majortype, mt.0.majortype) && wildcard(request.subtype, mt.0.subtype) && wildcard(request.formattype, mt.0.formattype))
        .collect()
}

/// # Safety
///
/// `mt` must own a VIDEOINFOHEADER or VIDEOINFOHEADER2 block.
unsafe fn negate_height(mt: &mut MediaType) {
    use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
    let offset = mt.0.cbFormat as usize - core::mem::size_of::<BITMAPINFOHEADER>();
    // SAFETY: The BITMAPINFOHEADER is the trailing member of both blocks.
    unsafe {
        let bmi = mt.0.pbFormat.add(offset).cast::<BITMAPINFOHEADER>();
        let mut header = bmi.read_unaligned();
        header.biHeight = -header.biHeight.abs();
        bmi.write_unaligned(header);
    }
}

impl IPin_Impl for OutputPin_Impl {
    fn Connect(&self, receivepin: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        guard(|| {
            let receiver = receivepin.ok()?;
            {
                let state = self.shared.state();
                if state.filter_state != State_Stopped {
                    return Err(VFW_E_NOT_STOPPED.into());
                }
                if state.connection.is_some() {
                    return Err(VFW_E_ALREADY_CONNECTED.into());
                }
            }
            // One graph at a time: a second consumer sees a busy device.
            let mut lease = match ConsumerLease::acquire(&DshowNames::for_camera(self.shared.camera.id)) {
                Ok(lease) => Some(lease),
                Err(LeaseError::Busy) => return Err(ERROR_BUSY.to_hresult().into()),
                Err(LeaseError::Unavailable) => return Err(E_UNEXPECTED.into()),
            };
            let offered = self.shared.offered();
            for mt in candidate_types(pmt, self.offered_types(), &offered) {
                if self.try_connect(receiver, mt, &offered, &mut lease).is_ok() {
                    return Ok(());
                }
            }
            Err(VFW_E_NO_ACCEPTABLE_TYPES.into())
        })
    }

    fn ReceiveConnection(&self, _connector: Ref<IPin>, _pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        // Output pins never receive connections.
        Err(E_UNEXPECTED.into())
    }

    fn Disconnect(&self) -> windows_core::Result<()> {
        guard(|| {
            let mut state = self.shared.state();
            if state.filter_state != State_Stopped {
                return Err(VFW_E_NOT_STOPPED.into());
            }
            if let Some(connection) = state.connection.take() {
                // SAFETY: Valid allocator interface.
                let _ = unsafe { connection.allocator.0.Decommit() };
            }
            Ok(())
        })
    }

    fn ConnectedTo(&self) -> windows_core::Result<IPin> {
        guard(|| self.shared.state().connection.as_ref().map(|c| c.peer.0.clone()).ok_or(VFW_E_NOT_CONNECTED.into()))
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> windows_core::Result<()> {
        guard(|| {
            if pmt.is_null() {
                return Err(E_POINTER.into());
            }
            match &self.shared.state().connection {
                Some(connection) => {
                    // SAFETY: Caller-provided writable AM_MEDIA_TYPE.
                    unsafe { connection.media_type.copy_to(pmt) };
                    Ok(())
                }
                None => {
                    // SAFETY: Documented to be zeroed when unconnected.
                    unsafe { core::ptr::write_bytes(pmt, 0, 1) };
                    Err(VFW_E_NOT_CONNECTED.into())
                }
            }
        })
    }

    fn QueryPinInfo(&self, pinfo: *mut PIN_INFO) -> windows_core::Result<()> {
        guard(|| {
            if pinfo.is_null() {
                return Err(E_POINTER.into());
            }
            let filter = self.filter.as_ref().and_then(Weak::upgrade).and_then(|unknown| unknown.cast::<IBaseFilter>().ok());
            // SAFETY: Caller-provided writable PIN_INFO; `pFilter` becomes caller-owned.
            unsafe {
                core::ptr::write(&mut (*pinfo).pFilter, core::mem::ManuallyDrop::new(filter));
                (*pinfo).dir = PINDIR_OUTPUT;
                (*pinfo).achName = pin_name();
            }
            Ok(())
        })
    }

    fn QueryDirection(&self) -> windows_core::Result<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> windows_core::Result<PWSTR> {
        guard(|| co_task_string(PIN_NAME))
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        guard_hr(|| {
            // SAFETY: COM passes null or a valid media type.
            let Some(parsed) = (unsafe { parse(pmt) }) else { return S_FALSE };
            match &self.shared.state().connection {
                Some(connection) if !parsed.matches(&connection.format) => S_FALSE,
                _ => S_OK,
            }
        })
    }

    fn EnumMediaTypes(&self) -> windows_core::Result<IEnumMediaTypes> {
        guard(|| Ok(MediaTypeEnumerator::create(Arc::new(self.offered_types()), 0)))
    }

    fn QueryInternalConnections(&self, _appin: OutRef<IPin>, _npin: *mut u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndOfStream(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn BeginFlush(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn EndFlush(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn NewSegment(&self, _tstart: i64, _tstop: i64, _drate: f64) -> windows_core::Result<()> {
        Ok(())
    }
}

impl IAMStreamConfig_Impl for OutputPin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        guard(|| {
            if pmt.is_null() {
                self.shared.state().preferred = None;
                return Ok(());
            }
            // SAFETY: COM passes a valid media type.
            let parsed = unsafe { parse(pmt) }.ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?;
            let offered = self.shared.offered();
            let base = *base_for(&offered, &parsed).ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?;
            let mut state = self.shared.state();
            if let Some(connection) = &state.connection {
                return if parsed.matches(&connection.format) { Ok(()) } else { Err(VFW_E_ALREADY_CONNECTED.into()) };
            }
            let format = parsed.to_format(&base);
            format.validate().map_err(|_| windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?;
            state.preferred = Some(format);
            Ok(())
        })
    }

    fn GetFormat(&self) -> windows_core::Result<*mut AM_MEDIA_TYPE> {
        guard(|| {
            let mt = self.offered_types().into_iter().next().ok_or(windows_core::Error::from(E_UNEXPECTED))?;
            let heap = mt.to_heap();
            if heap.is_null() { Err(windows::Win32::Foundation::E_OUTOFMEMORY.into()) } else { Ok(heap) }
        })
    }

    fn GetNumberOfCapabilities(&self, picount: *mut i32, pisize: *mut i32) -> windows_core::Result<()> {
        if picount.is_null() || pisize.is_null() {
            return Err(E_POINTER.into());
        }
        let connected = self.shared.state().connection.is_some();
        let count = if connected { 1 } else { self.shared.offered().len() as i32 };
        // SAFETY: Non-null caller out pointers.
        unsafe {
            *picount = count;
            *pisize = core::mem::size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
        }
        Ok(())
    }

    fn GetStreamCaps(&self, iindex: i32, ppmt: *mut *mut AM_MEDIA_TYPE, pscc: *mut u8) -> windows_core::Result<()> {
        guard(|| {
            if ppmt.is_null() || pscc.is_null() {
                return Err(E_POINTER.into());
            }
            // Bind first: the state guard must be released before
            // `offered()` locks the state again.
            let connected = self.shared.state().connection.as_ref().map(|c| c.format);
            let formats = connected.map_or_else(|| self.shared.offered(), |format| vec![format]);
            let format = *usize::try_from(iindex).ok().and_then(|index| formats.get(index)).ok_or(windows_core::Error::from(E_INVALIDARG))?;
            let mt = MediaType::new(&format, Header::VideoInfo).ok_or(windows_core::Error::from(E_UNEXPECTED))?;
            let size = SIZE {
                cx: format.width as i32,
                cy: format.height as i32,
            };
            let interval = format.frame_rate.interval_100ns().unwrap_or(0) as i64;
            let bits = (mt.0.lSampleSize as i64 * 8 * 10_000_000 / interval.max(1)).min(i64::from(i32::MAX)) as i32;
            let caps = VIDEO_STREAM_CONFIG_CAPS {
                guid: GUID::from_u128(guids::FORMAT_VIDEO_INFO),
                InputSize: size,
                MinCroppingSize: size,
                MaxCroppingSize: size,
                CropGranularityX: 1,
                CropGranularityY: 1,
                CropAlignX: 1,
                CropAlignY: 1,
                MinOutputSize: size,
                MaxOutputSize: size,
                OutputGranularityX: 1,
                OutputGranularityY: 1,
                MinFrameInterval: interval,
                MaxFrameInterval: interval,
                MinBitsPerSecond: bits,
                MaxBitsPerSecond: bits,
                ..VIDEO_STREAM_CONFIG_CAPS::default()
            };
            let heap = mt.to_heap();
            if heap.is_null() {
                return Err(windows::Win32::Foundation::E_OUTOFMEMORY.into());
            }
            // SAFETY: Non-null caller out pointers; `pscc` holds the size we
            // reported from GetNumberOfCapabilities.
            unsafe {
                *ppmt = heap;
                pscc.cast::<VIDEO_STREAM_CONFIG_CAPS>().write_unaligned(caps);
            }
            Ok(())
        })
    }
}

impl IKsPropertySet_Impl for OutputPin_Impl {
    fn Set(&self, _guidpropset: *const GUID, _dwpropid: u32, _pinstancedata: *const core::ffi::c_void, _cbinstancedata: u32, _ppropdata: *const core::ffi::c_void, _cbpropdata: u32) -> windows_core::Result<()> {
        Err(E_PROP_ID_UNSUPPORTED.into())
    }

    fn Get(&self, guidpropset: *const GUID, dwpropid: u32, _pinstancedata: *const core::ffi::c_void, _cbinstancedata: u32, ppropdata: *mut core::ffi::c_void, cbpropdata: u32, pcbreturned: *mut u32) -> windows_core::Result<()> {
        guard(|| {
            // SAFETY: COM passes null or a valid GUID.
            if unsafe { guidpropset.as_ref() } != Some(&GUID::from_u128(guids::AMPROPSETID_PIN)) {
                return Err(E_PROP_SET_UNSUPPORTED.into());
            }
            if dwpropid != AMPROPERTY_PIN_CATEGORY {
                return Err(E_PROP_ID_UNSUPPORTED.into());
            }
            if !pcbreturned.is_null() {
                // SAFETY: Non-null caller out pointer.
                unsafe { *pcbreturned = core::mem::size_of::<GUID>() as u32 };
            }
            if ppropdata.is_null() {
                // A null buffer queries the required size.
                return Ok(());
            }
            if (cbpropdata as usize) < core::mem::size_of::<GUID>() {
                return Err(E_UNEXPECTED.into());
            }
            // SAFETY: The caller's buffer holds at least one GUID.
            unsafe { ppropdata.cast::<GUID>().write_unaligned(GUID::from_u128(guids::PIN_CATEGORY_CAPTURE)) };
            Ok(())
        })
    }

    fn QuerySupported(&self, guidpropset: *const GUID, dwpropid: u32) -> windows_core::Result<u32> {
        // SAFETY: COM passes null or a valid GUID.
        if unsafe { guidpropset.as_ref() } != Some(&GUID::from_u128(guids::AMPROPSETID_PIN)) {
            return Err(E_PROP_SET_UNSUPPORTED.into());
        }
        if dwpropid != AMPROPERTY_PIN_CATEGORY {
            return Err(E_PROP_ID_UNSUPPORTED.into());
        }
        Ok(KSPROPERTY_SUPPORT_GET)
    }
}

/// `IEnumPins` over a fixed pin list.
#[implement(IEnumPins)]
pub(crate) struct PinEnumerator {
    pins: Vec<IPin>,
    position: Mutex<usize>,
    _module: ModuleRef,
}

impl PinEnumerator {
    pub(crate) fn create(pins: Vec<IPin>, position: usize) -> IEnumPins {
        Self {
            pins,
            position: Mutex::new(position),
            _module: ModuleRef::new(),
        }
        .into()
    }

    fn position(&self) -> std::sync::MutexGuard<'_, usize> {
        self.position.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl IEnumPins_Impl for PinEnumerator_Impl {
    fn Next(&self, cpins: u32, pppins: *mut Option<IPin>, pcfetched: *mut u32) -> HRESULT {
        guard_hr(|| {
            if pppins.is_null() || (cpins != 1 && pcfetched.is_null()) {
                return E_POINTER;
            }
            let mut position = self.position();
            let mut fetched = 0_u32;
            while fetched < cpins && *position < self.pins.len() {
                // SAFETY: The caller provides `cpins` writable slots.
                unsafe { pppins.add(fetched as usize).write(Some(self.pins[*position].clone())) };
                *position += 1;
                fetched += 1;
            }
            if !pcfetched.is_null() {
                // SAFETY: Non-null caller out pointer.
                unsafe { *pcfetched = fetched };
            }
            if fetched == cpins { S_OK } else { S_FALSE }
        })
    }

    fn Skip(&self, cpins: u32) -> windows_core::Result<()> {
        let mut position = self.position();
        *position = position.saturating_add(cpins as usize);
        if *position <= self.pins.len() { Ok(()) } else { Err(S_FALSE.into()) }
    }

    fn Reset(&self) -> windows_core::Result<()> {
        *self.position() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IEnumPins> {
        Ok(PinEnumerator::create(self.pins.clone(), *self.position()))
    }
}

/// `IEnumMediaTypes` over a snapshot of offered types.
#[implement(IEnumMediaTypes)]
pub(crate) struct MediaTypeEnumerator {
    types: Arc<Vec<MediaType>>,
    position: Mutex<usize>,
    _module: ModuleRef,
}

impl MediaTypeEnumerator {
    pub(crate) fn create(types: Arc<Vec<MediaType>>, position: usize) -> IEnumMediaTypes {
        Self {
            types,
            position: Mutex::new(position),
            _module: ModuleRef::new(),
        }
        .into()
    }

    fn position(&self) -> std::sync::MutexGuard<'_, usize> {
        self.position.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl IEnumMediaTypes_Impl for MediaTypeEnumerator_Impl {
    fn Next(&self, cmediatypes: u32, ppmediatypes: *mut *mut AM_MEDIA_TYPE, pcfetched: *mut u32) -> HRESULT {
        guard_hr(|| {
            if ppmediatypes.is_null() || (cmediatypes != 1 && pcfetched.is_null()) {
                return E_POINTER;
            }
            let mut position = self.position();
            let mut fetched = 0_u32;
            while fetched < cmediatypes && *position < self.types.len() {
                let heap = self.types[*position].to_heap();
                if heap.is_null() {
                    break;
                }
                // SAFETY: The caller provides `cmediatypes` writable slots.
                unsafe { ppmediatypes.add(fetched as usize).write(heap) };
                *position += 1;
                fetched += 1;
            }
            if !pcfetched.is_null() {
                // SAFETY: Non-null caller out pointer.
                unsafe { *pcfetched = fetched };
            }
            if fetched == cmediatypes { S_OK } else { S_FALSE }
        })
    }

    fn Skip(&self, cmediatypes: u32) -> windows_core::Result<()> {
        let mut position = self.position();
        *position = position.saturating_add(cmediatypes as usize);
        if *position <= self.types.len() { Ok(()) } else { Err(S_FALSE.into()) }
    }

    fn Reset(&self) -> windows_core::Result<()> {
        *self.position() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IEnumMediaTypes> {
        Ok(MediaTypeEnumerator::create(Arc::clone(&self.types), *self.position()))
    }
}
