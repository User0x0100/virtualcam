use core::ptr::{NonNull, addr_of, addr_of_mut};
use core::sync::atomic::{Ordering, compiler_fence, fence};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, OpenFileMappingW, PAGE_READWRITE};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use super::FrameSource;
use super::protocol::managed::{CONTROL_STATE_READY, CONTROL_STATE_STOPPING, ManagedControlHeader};
use super::protocol::obs::{QueueHeader, QueueLayout, SLOT_COUNT, STATE_READY, STATE_STARTING, STATE_STOPPING};
use crate::config::ValidatedObsOptions;
use crate::platform::windows::{MappedView, OwnedHandle, last_error};
use crate::{CameraType, Error, Result, WriteStatus};

pub(super) const OBS_CLSID_KEY: &str = "CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";

fn managed_object_name(video_name: &[u16], suffix: &str) -> Vec<u16> {
    let end = video_name.iter().position(|unit| *unit == 0).unwrap_or(video_name.len());
    let mut name = video_name[..end].to_vec();
    name.extend(suffix.encode_utf16());
    name.push(0);
    name
}

fn managed_ready_event_name(video_name: &[u16]) -> Vec<u16> {
    managed_object_name(video_name, "-Ready")
}

fn managed_producer_claim_name(video_name: &[u16]) -> Vec<u16> {
    managed_object_name(video_name, "-Producer")
}

fn managed_control_name(video_name: &[u16]) -> Vec<u16> {
    managed_object_name(video_name, "-Control")
}

fn managed_data_name(video_name: &[u16], options: ValidatedObsOptions) -> Vec<u16> {
    managed_object_name(video_name, &format!("-Data-{}x{}-{}", options.width, options.height, options.interval_100ns))
}

fn create_managed_producer_claim(video_name: &[u16]) -> Result<OwnedHandle> {
    let claim_name = managed_producer_claim_name(video_name);
    // Only producers open this named object. Its lifetime is therefore an exact
    // single-writer lease. It remains unsignaled until the complete producer
    // session has been published.
    // SAFETY: `claim_name` is a stable NUL-terminated UTF-16 object name.
    let raw_claim = unsafe { CreateEventW(core::ptr::null(), 1, 0, claim_name.as_ptr()) };
    if raw_claim.is_null() {
        return Err(last_error("CreateEventW(managed producer claim)"));
    }
    // SAFETY: Read immediately after successful creation.
    let create_status = unsafe { GetLastError() };
    // SAFETY: Successful CreateEventW transferred one owned handle.
    let claim = unsafe { OwnedHandle::from_raw(raw_claim) };
    if create_status == ERROR_ALREADY_EXISTS {
        return Err(Error::DeviceInUse);
    }
    Ok(claim)
}

fn open_managed_ready_event(video_name: &[u16]) -> Result<OwnedHandle> {
    let ready_name = managed_ready_event_name(video_name);
    // Ready is a reusable notification object, not an ownership token. A
    // DirectShow consumer may retain it across producer sessions.
    // SAFETY: `ready_name` is a stable NUL-terminated UTF-16 object name.
    let raw_event = unsafe { CreateEventW(core::ptr::null(), 1, 0, ready_name.as_ptr()) };
    if raw_event.is_null() {
        return Err(last_error("CreateEventW(managed runtime ready)"));
    }
    // SAFETY: Successful CreateEventW transferred one owned handle.
    let event = unsafe { OwnedHandle::from_raw(raw_event) };
    // Clear a stale signaled state before invalidating/publishing the new control
    // header. The producer claim is still unsignaled during this transition.
    // SAFETY: `event` is a live manual-reset event with modify-state access.
    if unsafe { ResetEvent(event.raw()) } == 0 {
        return Err(last_error("ResetEvent(managed runtime ready)"));
    }
    Ok(event)
}

fn open_managed_control(video_name: &[u16], camera_type: CameraType) -> Result<(OwnedHandle, MappedView, NonNull<ManagedControlHeader>)> {
    let control_name = managed_control_name(video_name);
    let control_size = u32::try_from(core::mem::size_of::<ManagedControlHeader>()).map_err(|_| Error::FrameSizeOverflow)?;
    // SAFETY: Page-file-backed fixed-size control mapping with a stable name.
    let raw_mapping = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, core::ptr::null(), PAGE_READWRITE, 0, control_size, control_name.as_ptr()) };
    if raw_mapping.is_null() {
        return Err(last_error("CreateFileMappingW(managed control)"));
    }
    // SAFETY: Successful CreateFileMappingW transferred one owned handle.
    let mapping = unsafe { OwnedHandle::from_raw(raw_mapping) };
    let view = MappedView::map(mapping.raw(), FILE_MAP_ALL_ACCESS, "MapViewOfFile(managed control)")?;
    let header = view.ptr_at::<ManagedControlHeader>(0).ok_or(Error::Protocol {
        backend: camera_type,
        detail: "managed control mapping does not provide an aligned control header",
    })?;
    // Invalidate stale session metadata immediately. No consumer treats the new
    // session as live until Ready and the producer claim are signaled later.
    // SAFETY: `header` is aligned writable storage inside the mapped view.
    unsafe {
        core::ptr::write_bytes(header.as_ptr().cast::<u8>(), 0, core::mem::size_of::<ManagedControlHeader>());
    }
    fence(Ordering::SeqCst);
    Ok((mapping, view, header))
}

struct ManagedRuntime {
    // Keep the control storage alive for `control_header`. Fields are ordered so
    // the producer claim is dropped last if construction unwinds or the runtime
    // is released normally.
    _control_view: MappedView,
    control_header: NonNull<ManagedControlHeader>,
    _control_mapping: OwnedHandle,
    ready_event: OwnedHandle,
    producer_claim: OwnedHandle,
}

impl ManagedRuntime {
    fn reserve(video_name: &[u16], camera_type: CameraType) -> Result<Self> {
        let producer_claim = create_managed_producer_claim(video_name)?;
        let ready_event = open_managed_ready_event(video_name)?;
        let (control_mapping, control_view, control_header) = open_managed_control(video_name, camera_type)?;
        Ok(Self {
            _control_view: control_view,
            control_header,
            _control_mapping: control_mapping,
            ready_event,
            producer_claim,
        })
    }

    fn publish(&self, options: ValidatedObsOptions) -> Result<()> {
        let control = ManagedControlHeader::new(options.width, options.height, options.interval_100ns);
        // SAFETY: `reserve` returned aligned writable storage and the caller only
        // publishes after the data queue and all other fallible state are ready.
        unsafe {
            core::ptr::copy_nonoverlapping(core::ptr::from_ref(&control), self.control_header.as_ptr(), 1);
        }
        fence(Ordering::SeqCst);
        // SAFETY: Both are live manual-reset events owned by this runtime.
        if unsafe { SetEvent(self.ready_event.raw()) } == 0 {
            return Err(last_error("SetEvent(managed runtime ready)"));
        }
        // Existence reserves ownership; signaling the claim is the final commit
        // point that tells native consumers the session is complete.
        if unsafe { SetEvent(self.producer_claim.raw()) } == 0 {
            return Err(last_error("SetEvent(managed producer claim)"));
        }
        Ok(())
    }

    fn set_state(&self, state: u32) {
        // SAFETY: The control mapping remains alive for this runtime's lifetime.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*self.control_header.as_ptr()).state), state);
        }
    }
}

pub(crate) struct ObsBackend {
    camera_type: CameraType,
    view: Option<MappedView>,
    header: Option<NonNull<QueueHeader>>,
    mapping: Option<OwnedHandle>,
    managed: Option<ManagedRuntime>,
    layout: QueueLayout,
    y_plane_size: usize,
    clock_frequency: u64,
    open: bool,
}

impl ObsBackend {
    pub(crate) fn open(options: ValidatedObsOptions, video_name: &[u16], camera_type: CameraType) -> Result<Self> {
        let layout = QueueLayout::calculate(options.frame_size)?;
        let managed_data_name = (camera_type == CameraType::ManagedNv12).then(|| managed_data_name(video_name, options));
        let data_name = managed_data_name.as_deref().unwrap_or(video_name);
        let managed = if camera_type == CameraType::ManagedNv12 { Some(ManagedRuntime::reserve(video_name, camera_type)?) } else { None };

        if !options.force_open && camera_type != CameraType::ManagedNv12 {
            // OBS has no separate producer lease, so preserve its upstream
            // conservative mapping-existence occupancy rule.
            // SAFETY: `video_name` is a NUL-terminated UTF-16 object name.
            let existing = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, video_name.as_ptr()) };
            if !existing.is_null() {
                // SAFETY: A successful OpenFileMappingW returns one owned handle.
                drop(unsafe { OwnedHandle::from_raw(existing) });
                trace_warn!("OBS queue mapping already exists");
                return Err(Error::DeviceInUse);
            }
        } else if options.force_open {
            trace_warn!("OBS force_open bypassed the conservative occupancy check");
        }

        let mapping_size = u32::try_from(layout.total_size).map_err(|_| Error::FrameSizeOverflow)?;
        // SAFETY: INVALID_HANDLE_VALUE selects a page-file-backed mapping;
        // protection, 32-bit size, and static NUL-terminated name match upstream.
        let raw_mapping = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, mapping_size, data_name.as_ptr()) };
        if raw_mapping.is_null() {
            return Err(last_error("CreateFileMappingW(OBSVirtualCamVideo)"));
        }
        // GetLastError must be captured immediately after successful creation.
        // SAFETY: `GetLastError` reads thread-local state without preconditions.
        let create_status = unsafe { GetLastError() };
        // SAFETY: Successful CreateFileMappingW transferred one owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(raw_mapping) };
        let already_exists = create_status == ERROR_ALREADY_EXISTS;

        // OBS still uses mapping existence as its writer-occupancy signal.
        // ManagedNv12 has a separate producer lease and a format-specific data
        // mapping name, so a retained same-format consumer section is safe to
        // reinitialize while different formats never share section capacity.
        if already_exists && !options.force_open && camera_type != CameraType::ManagedNv12 {
            return Err(Error::DeviceInUse);
        }

        let view = MappedView::map(mapping.raw(), FILE_MAP_ALL_ACCESS, "MapViewOfFile(OBSVirtualCamVideo)")?;
        if already_exists && view.accessible_len() < layout.total_size {
            return Err(Error::ExistingMappingTooSmall {
                required: layout.total_size,
                available: view.accessible_len(),
            });
        }
        if view.accessible_len() < layout.total_size {
            return Err(Error::Protocol {
                backend: camera_type,
                detail: "new NV12 queue mapping exposes less memory than requested",
            });
        }

        let mapped_header = view.ptr_at::<QueueHeader>(0).ok_or(Error::Protocol {
            backend: camera_type,
            detail: "NV12 queue mapping does not provide an aligned queue header",
        })?;
        let mut header = QueueHeader::new(options.width, options.height, options.interval_100ns, layout.offsets);
        header.state = 0;
        // SAFETY: `ptr_at` checked alignment and object bounds, and `header` does
        // not alias the writable mapped destination.
        unsafe {
            core::ptr::copy_nonoverlapping(core::ptr::from_ref(&header), mapped_header.as_ptr(), 1);
        }
        fence(Ordering::SeqCst);
        // Publish STARTING only after every format/layout field is initialized.
        // Consumers may observe the mapping name before this store completes.
        // SAFETY: `mapped_header` is the checked live writable queue header.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*mapped_header.as_ptr()).state), STATE_STARTING);
        }
        fence(Ordering::SeqCst);

        let mut frequency = 0_i64;
        // SAFETY: `frequency` is valid writable storage for LARGE_INTEGER's i64
        // representation used by windows-sys.
        if unsafe { QueryPerformanceFrequency(&raw mut frequency) } == 0 {
            return Err(last_error("QueryPerformanceFrequency"));
        }
        let clock_frequency = u64::try_from(frequency).map_err(|_| Error::Protocol {
            backend: camera_type,
            detail: "performance counter frequency is not positive",
        })?;

        if let Some(runtime) = managed.as_ref() {
            runtime.publish(options)?;
        }

        if already_exists {
            trace_warn!(required = layout.total_size, available = view.accessible_len(), camera_type = ?camera_type, "NV12 producer reinitialized an existing same-format mapping");
        }

        Ok(Self {
            camera_type,
            view: Some(view),
            header: Some(mapped_header),
            mapping: Some(mapping),
            managed,
            layout,
            y_plane_size: usize::try_from(options.width)
                .ok()
                .and_then(|width| usize::try_from(options.height).ok().and_then(|height| width.checked_mul(height)))
                .ok_or(Error::FrameSizeOverflow)?,
            clock_frequency,
            open: true,
        })
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        // OBS exposes no receiver-to-writer presence signal. A live queue is
        // protocol-ready regardless of whether a filter currently maps it.
        Ok(true)
    }

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        let byte_len = frame.byte_len();
        if byte_len != self.layout.frame_size {
            return Err(Error::FrameSizeMismatch {
                expected: self.layout.frame_size,
                actual: byte_len,
            });
        }
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        let view = self.view.as_ref().ok_or(Error::AlreadyClosed)?;
        let header = self.header.ok_or(Error::AlreadyClosed)?.as_ptr();
        let timestamp = self.timestamp_ns()?;

        let y_size = self.y_plane_size;
        let uv_size = y_size / 2;
        if y_size.checked_add(uv_size) != Some(self.layout.frame_size) {
            return Err(Error::Protocol {
                backend: self.camera_type,
                detail: "configured NV12 plane sizes do not match the frame size",
            });
        }

        // SAFETY: `header` points to the initialized queue header in our writable
        // mapping. The field matches C's volatile uint32_t and is within the view.
        let current = unsafe { core::ptr::read_volatile(addr_of!((*header).write_idx)) };
        let increment = current.wrapping_add(1);
        // SAFETY: Same mapped volatile field as above; this is the upstream first
        // publication step (`++qh->write_idx`).
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*header).write_idx), increment);
        }
        compiler_fence(Ordering::SeqCst);

        let slot = usize::try_from(increment % u32::try_from(SLOT_COUNT).map_err(|_| Error::FrameSizeOverflow)?).map_err(|_| Error::FrameSizeOverflow)?;
        let slot_offset = usize::try_from(self.layout.offsets[slot]).map_err(|_| Error::FrameSizeOverflow)?;
        let timestamp_ptr = view.ptr_at::<u64>(slot_offset).ok_or(Error::Protocol {
            backend: self.camera_type,
            detail: "NV12 queue slot timestamp is outside or misaligned in the mapping",
        })?;

        // SAFETY: `ptr_at` proved the timestamp is aligned and writable;
        // QueueLayout proved the contiguous frame payload follows inside the view.
        let destination = unsafe {
            core::ptr::write(timestamp_ptr.as_ptr(), timestamp);
            view.as_ptr().add(slot_offset + 32)
        };
        frame.copy_to(destination)?;

        compiler_fence(Ordering::SeqCst);
        // SAFETY: These are the initialized volatile publication fields in the
        // writable header. Their order mirrors `video_queue_write`: read_idx,
        // then state READY, after timestamp and both plane copies complete.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*header).read_idx), increment);
            core::ptr::write_volatile(addr_of_mut!((*header).state), STATE_READY);
        }
        if let Some(runtime) = self.managed.as_ref() {
            runtime.set_state(CONTROL_STATE_READY);
        }
        Ok(WriteStatus::Written)
    }

    fn timestamp_ns(&self) -> Result<u64> {
        let mut counter = 0_i64;
        // SAFETY: `counter` is writable storage for the API's LARGE_INTEGER.
        if unsafe { QueryPerformanceCounter(&raw mut counter) } == 0 {
            return Err(last_error("QueryPerformanceCounter"));
        }
        let counter = u64::try_from(counter).map_err(|_| Error::Protocol {
            backend: self.camera_type,
            detail: "performance counter returned a negative value",
        })?;
        Ok(mul_div_u64(counter, 1_000_000_000, self.clock_frequency))
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: The checked data header remains live and writable until its
        // view is released below. Publish both data/control STOPPING before
        // releasing producer-owned transport handles.
        unsafe {
            if let Some(header) = self.header {
                core::ptr::write_volatile(addr_of_mut!((*header.as_ptr()).state), STATE_STOPPING);
            }
        }
        if let Some(runtime) = self.managed.as_ref() {
            runtime.set_state(CONTROL_STATE_STOPPING);
        }
        self.open = false;
        self.header = None;
        self.view = None;
        self.mapping = None;
        // Drop managed runtime last: its control view is released before its
        // mapping handle, and its producer claim is the final owned handle.
        self.managed = None;
        Ok(())
    }
}

impl Drop for ObsBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn mul_div_u64(number: u64, multiplier: u64, divisor: u64) -> u64 {
    let scaled = (u128::from(number) * u128::from(multiplier)) / u128::from(divisor);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_READ, OpenFileMappingW, PAGE_READWRITE};
    use windows_sys::Win32::System::Threading::{CreateEventW, OpenEventW, WaitForSingleObject};

    use super::*;
    use crate::{ObsFrameRate, ObsOptions};

    fn test_name(suffix: &str) -> Vec<u16> {
        let name = format!("Local\\VirtualCamRustObs_{}_{}", std::process::id(), suffix);
        name.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn validated(width: u32, height: u32, force_open: bool) -> ValidatedObsOptions {
        ObsOptions {
            width,
            height,
            frame_rate: ObsFrameRate::default(),
            force_open,
        }
        .validate()
        .expect("valid OBS test configuration")
    }

    #[test]
    fn timestamp_conversion_matches_upstream_mul_div() {
        assert_eq!(mul_div_u64(12_345, 1_000_000_000, 10_000_000), 1_234_500);
    }

    #[test]
    fn managed_producer_claim_reserves_writer_before_mapping_exists() {
        let name = test_name("managed_producer_reservation");
        let claim_name = managed_producer_claim_name(&name);
        // SAFETY: unique NUL-terminated test name; the returned handle is owned.
        let raw_event = unsafe { CreateEventW(core::ptr::null(), 1, 0, claim_name.as_ptr()) };
        assert!(!raw_event.is_null());
        // SAFETY: Successful CreateEventW transferred one owned handle.
        let _claim = unsafe { OwnedHandle::from_raw(raw_event) };

        assert!(matches!(ObsBackend::open(validated(64, 64, false), &name, CameraType::ManagedNv12), Err(Error::DeviceInUse)));
    }

    #[test]
    fn managed_reopen_allows_consumer_held_objects_and_larger_next_format() {
        let name = test_name("managed_consumer_holds_objects");
        let ready_name = managed_ready_event_name(&name);
        let control_name = managed_control_name(&name);
        let first_config = validated(64, 64, false);
        let first_data_name = managed_data_name(&name, first_config);
        let mut first = ObsBackend::open(first_config, &name, CameraType::ManagedNv12).expect("first managed producer opens");

        // Model a DirectShow consumer that retains the old transport objects.
        // The next producer must not be constrained by this old data section's
        // capacity because data mappings are format-specific in managed ABI v3.
        const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
        // SAFETY: names are stable NUL-terminated test-only kernel object names.
        let raw_ready = unsafe { OpenEventW(SYNCHRONIZE_ACCESS, 0, ready_name.as_ptr()) };
        assert!(!raw_ready.is_null());
        // SAFETY: successful OpenEventW returned one owned handle.
        let held_ready = unsafe { OwnedHandle::from_raw(raw_ready) };
        // SAFETY: stable read-only mapping names model retained consumer handles.
        let raw_control = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, control_name.as_ptr()) };
        assert!(!raw_control.is_null());
        let _held_control = unsafe { OwnedHandle::from_raw(raw_control) };
        let raw_mapping = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, first_data_name.as_ptr()) };
        assert!(!raw_mapping.is_null());
        let _held_old_data = unsafe { OwnedHandle::from_raw(raw_mapping) };

        assert!(matches!(ObsBackend::open(first_config, &name, CameraType::ManagedNv12), Err(Error::DeviceInUse)));

        first.close().expect("first managed producer closes");
        let second_config = validated(1920, 1080, false);
        let mut second = ObsBackend::open(second_config, &name, CameraType::ManagedNv12).expect("larger producer opens beside retained old data section");
        // The reopened producer reset and then re-signaled the reusable Ready event.
        // SAFETY: `held_ready` is a live SYNCHRONIZE handle.
        assert_eq!(unsafe { WaitForSingleObject(held_ready.raw(), 0) }, WAIT_OBJECT_0);
        second.close().expect("second managed producer closes");
    }

    #[test]
    fn real_mapping_covers_occupancy_force_write_and_stopping() {
        let name = test_name("state");
        let config = validated(2, 2, false);
        let mut original = ObsBackend::open(config, &name, CameraType::Obs).expect("first writer opens a new mapping");

        assert!(matches!(ObsBackend::open(config, &name, CameraType::Obs), Err(Error::DeviceInUse)));

        let mut forced = ObsBackend::open(validated(2, 2, true), &name, CameraType::Obs).expect("force mode reuses a large enough mapping");
        let observer = original.view.as_ref().expect("original view remains mapped");
        let header = observer.as_ptr().cast::<QueueHeader>();
        // SAFETY: `observer` maps the same live initialized queue header.
        assert_eq!(unsafe { core::ptr::read_volatile(addr_of!((*header).state)) }, 1);

        let frame = [1_u8, 2, 3, 4, 5, 6];
        assert!(matches!(forced.write_frame(FrameSource::Host(&frame[..5])), Err(Error::FrameSizeMismatch { expected: 6, actual: 5 })));
        // A rejected frame must not advance or publish queue state.
        // SAFETY: `header` remains a live initialized queue header.
        unsafe {
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).write_idx)), 0);
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).read_idx)), 0);
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).state)), 1);
        }

        // Exercise the backend's real volatile increment at u32 wraparound.
        // SAFETY: `header` remains a live initialized queue header.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*header).write_idx), u32::MAX);
        }
        assert_eq!(forced.write_frame(FrameSource::Host(&frame)), Ok(WriteStatus::Written));
        // SAFETY: All fields and the selected slot are in the observer's live view.
        unsafe {
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).write_idx)), 0);
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).read_idx)), 0);
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).state)), STATE_READY);
            let slot = (*header).offsets[0] as usize;
            assert!(core::ptr::read(observer.as_ptr().add(slot).cast::<u64>()) > 0);
            assert_eq!(core::slice::from_raw_parts(observer.as_ptr().add(slot + 32), frame.len()), frame);
        }

        forced.close().expect("forced writer closes");
        forced.close().expect("close is idempotent");
        // SAFETY: `original` keeps an independent mapped view live after forced
        // close and can observe the required STOPPING publication.
        assert_eq!(unsafe { core::ptr::read_volatile(addr_of!((*header).state)) }, STATE_STOPPING);
        original.close().expect("original writer closes");
    }

    #[test]
    fn force_open_rejects_a_small_existing_mapping() {
        let name = test_name("small");
        // SAFETY: Creates a page-file-backed test mapping with a unique name and
        // a deliberately insufficient one-page extent.
        let raw = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, name.as_ptr()) };
        assert!(!raw.is_null());
        // SAFETY: Successful creation returned one owned mapping handle.
        let _mapping = unsafe { OwnedHandle::from_raw(raw) };
        let error = match ObsBackend::open(validated(64, 64, true), &name, CameraType::Obs) {
            Ok(_) => panic!("small mapping must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            Error::ExistingMappingTooSmall {
                required,
                available
            } if required > available
        ));
    }
}
