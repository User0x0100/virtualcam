use core::{
    ptr::{addr_of, addr_of_mut},
    sync::atomic::{Ordering, compiler_fence},
};

use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE},
    System::{
        Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, FILE_MAP_READ, OpenFileMappingW,
            PAGE_READWRITE,
        },
        Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    },
};

use crate::{
    CameraType, Error, OpenOptions, Result, WriteStatus,
    config::ValidatedConfig,
    platform::windows::{MappedView, OwnedHandle, last_error, registry_key_exists},
};

use super::protocol::obs::{
    QueueHeader, QueueLayout, SLOT_COUNT, STATE_READY, STATE_STOPPING, VIDEO_NAME,
};

const OBS_CLSID_KEY: &str = "CLSID\\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}";

pub(crate) struct ObsBackend {
    view: Option<MappedView>,
    mapping: Option<OwnedHandle>,
    layout: QueueLayout,
    y_plane_size: usize,
    clock_frequency: u64,
    open: bool,
}

impl ObsBackend {
    pub(crate) fn open(config: ValidatedConfig, options: OpenOptions) -> Result<Self> {
        if !registry_key_exists(OBS_CLSID_KEY) {
            return Err(Error::BackendUnavailable(CameraType::Obs));
        }
        Self::open_protocol(config, options, VIDEO_NAME)
    }

    fn open_protocol(
        config: ValidatedConfig,
        options: OpenOptions,
        video_name: &[u16],
    ) -> Result<Self> {
        let layout = QueueLayout::calculate(config.frame_size)?;
        if !options.force_open {
            // Upstream performs this conservative read-only open first and treats
            // any successful handle as an occupied producer queue.
            // SAFETY: `video_name` is a NUL-terminated UTF-16 object name.
            let existing = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, video_name.as_ptr()) };
            if !existing.is_null() {
                // SAFETY: A successful OpenFileMappingW returns one owned handle.
                drop(unsafe { OwnedHandle::from_raw(existing) });
                trace_warn!("OBS queue mapping already exists");
                return Err(Error::DeviceInUse);
            }
        } else {
            trace_warn!("OBS force_open bypassed the conservative occupancy check");
        }

        let mapping_size =
            u32::try_from(layout.total_size).map_err(|_| Error::FrameSizeOverflow)?;
        // SAFETY: INVALID_HANDLE_VALUE selects a page-file-backed mapping;
        // protection, 32-bit size, and static NUL-terminated name match upstream.
        let raw_mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                mapping_size,
                video_name.as_ptr(),
            )
        };
        if raw_mapping.is_null() {
            return Err(last_error("CreateFileMappingW(OBSVirtualCamVideo)"));
        }
        // GetLastError must be captured immediately after successful creation.
        // SAFETY: `GetLastError` reads thread-local state without preconditions.
        let create_status = unsafe { GetLastError() };
        // SAFETY: Successful CreateFileMappingW transferred one owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(raw_mapping) };
        let already_exists = create_status == ERROR_ALREADY_EXISTS;

        // A mapping appearing in the normal-mode race window is still occupied.
        // This preserves the advertised single-writer contract and avoids
        // initializing an unvalidated foreign mapping.
        if already_exists && !options.force_open {
            return Err(Error::DeviceInUse);
        }

        let view = MappedView::map(
            mapping.raw(),
            FILE_MAP_ALL_ACCESS,
            "MapViewOfFile(OBSVirtualCamVideo)",
        )?;
        if already_exists && view.accessible_len() < layout.total_size {
            return Err(Error::ExistingMappingTooSmall {
                required: layout.total_size,
                available: view.accessible_len(),
            });
        }
        if view.accessible_len() < layout.total_size {
            return Err(Error::Protocol {
                backend: CameraType::Obs,
                detail: "new OBS mapping exposes less memory than requested",
            });
        }

        let header = QueueHeader::new(
            config.width,
            config.height,
            config.obs_interval_100ns,
            layout.offsets,
        );
        // SAFETY: The mapped view is writable and at least `layout.total_size`
        // bytes long. Its base has allocation alignment, which exceeds the
        // QueueHeader alignment. `header` does not alias the destination.
        unsafe {
            core::ptr::copy_nonoverlapping(&header, view.as_ptr().cast::<QueueHeader>(), 1);
        }

        let mut frequency = 0_i64;
        // SAFETY: `frequency` is valid writable storage for LARGE_INTEGER's i64
        // representation used by windows-sys.
        if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 {
            return Err(last_error("QueryPerformanceFrequency"));
        }
        let clock_frequency = u64::try_from(frequency).map_err(|_| Error::Protocol {
            backend: CameraType::Obs,
            detail: "performance counter frequency is not positive",
        })?;

        if already_exists {
            trace_warn!(
                required = layout.total_size,
                available = view.accessible_len(),
                "OBS force_open reinitialized an existing mapping"
            );
        }

        Ok(Self {
            view: Some(view),
            mapping: Some(mapping),
            layout,
            y_plane_size: usize::try_from(config.width)
                .ok()
                .and_then(|width| {
                    usize::try_from(config.height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
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

    pub(crate) fn write_host_frame(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        if frame.len() != self.layout.frame_size {
            return Err(Error::FrameSizeMismatch {
                expected: self.layout.frame_size,
                actual: frame.len(),
            });
        }
        let view = self.view.as_ref().ok_or(Error::AlreadyClosed)?;
        let timestamp = self.timestamp_ns()?;
        let header = view.as_ptr().cast::<QueueHeader>();

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

        let slot = (increment % SLOT_COUNT as u32) as usize;
        let slot_offset = self.layout.offsets[slot] as usize;
        let y_size = self.y_plane_size;
        let uv_size = y_size / 2;
        if y_size.checked_add(uv_size) != Some(frame.len()) {
            return Err(Error::Protocol {
                backend: CameraType::Obs,
                detail: "configured NV12 plane sizes do not match the frame size",
            });
        }

        // SAFETY: QueueLayout proved the timestamp header and full frame slot lie
        // within the mapped view. NV12 validation proved `frame` contains `y_size`
        // Y bytes followed by `uv_size` UV bytes. Source and mapping do not overlap.
        unsafe {
            let timestamp_ptr = view.as_ptr().add(slot_offset).cast::<u64>();
            core::ptr::write(timestamp_ptr, timestamp);
            let destination = view.as_ptr().add(slot_offset + 32);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), destination, y_size);
            core::ptr::copy_nonoverlapping(
                frame.as_ptr().add(y_size),
                destination.add(y_size),
                uv_size,
            );
        }

        compiler_fence(Ordering::SeqCst);
        // SAFETY: These are the initialized volatile publication fields in the
        // writable header. Their order mirrors `video_queue_write`: read_idx,
        // then state READY, after timestamp and both plane copies complete.
        unsafe {
            core::ptr::write_volatile(addr_of_mut!((*header).read_idx), increment);
            core::ptr::write_volatile(addr_of_mut!((*header).state), STATE_READY);
        }
        Ok(WriteStatus::Written)
    }

    fn timestamp_ns(&self) -> Result<u64> {
        let mut counter = 0_i64;
        // SAFETY: `counter` is writable storage for the API's LARGE_INTEGER.
        if unsafe { QueryPerformanceCounter(&mut counter) } == 0 {
            return Err(last_error("QueryPerformanceCounter"));
        }
        let counter = u64::try_from(counter).map_err(|_| Error::Protocol {
            backend: CameraType::Obs,
            detail: "performance counter returned a negative value",
        })?;
        Ok(mul_div_u64(counter, 1_000_000_000, self.clock_frequency))
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        if let Some(view) = self.view.as_ref() {
            let header = view.as_ptr().cast::<QueueHeader>();
            compiler_fence(Ordering::SeqCst);
            // SAFETY: The mapped header remains live and writable until the view
            // is taken below. This is the writer-only upstream close transition.
            unsafe {
                core::ptr::write_volatile(addr_of_mut!((*header).state), STATE_STOPPING);
            }
        }
        self.open = false;
        self.view = None;
        self.mapping = None;
        Ok(())
    }
}

impl Drop for ObsBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn mul_div_u64(number: u64, multiplier: u64, divisor: u64) -> u64 {
    ((number as u128 * multiplier as u128) / divisor as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, PAGE_READWRITE};

    use crate::{CameraConfig, config::CameraType};

    fn test_name(suffix: &str) -> Vec<u16> {
        let name = format!("Local\\VirtualCamRustObs_{}_{}", std::process::id(), suffix);
        name.encode_utf16().chain(core::iter::once(0)).collect()
    }

    fn validated(width: u32, height: u32) -> ValidatedConfig {
        CameraConfig {
            width,
            height,
            fps: 30.0,
        }
        .validate(CameraType::Obs)
        .expect("valid OBS test configuration")
    }

    #[test]
    fn timestamp_conversion_matches_upstream_mul_div() {
        assert_eq!(mul_div_u64(12_345, 1_000_000_000, 10_000_000), 1_234_500);
    }

    #[test]
    fn real_mapping_covers_occupancy_force_write_and_stopping() {
        let name = test_name("state");
        let config = validated(2, 2);
        let mut original = ObsBackend::open_protocol(config, OpenOptions::default(), &name)
            .expect("first writer opens a new mapping");

        assert!(matches!(
            ObsBackend::open_protocol(config, OpenOptions::default(), &name),
            Err(Error::DeviceInUse)
        ));

        let mut forced = ObsBackend::open_protocol(config, OpenOptions { force_open: true }, &name)
            .expect("force mode reuses a large enough mapping");
        let observer = original
            .view
            .as_ref()
            .expect("original view remains mapped");
        let header = observer.as_ptr().cast::<QueueHeader>();
        // SAFETY: `observer` maps the same live initialized queue header.
        assert_eq!(
            unsafe { core::ptr::read_volatile(addr_of!((*header).state)) },
            1
        );

        let frame = [1_u8, 2, 3, 4, 5, 6];
        assert_eq!(forced.write_host_frame(&frame), Ok(WriteStatus::Written));
        // SAFETY: All fields and the selected slot are in the observer's live view.
        unsafe {
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).write_idx)), 1);
            assert_eq!(core::ptr::read_volatile(addr_of!((*header).read_idx)), 1);
            assert_eq!(
                core::ptr::read_volatile(addr_of!((*header).state)),
                STATE_READY
            );
            let slot = (*header).offsets[1] as usize;
            assert!(core::ptr::read(observer.as_ptr().add(slot).cast::<u64>()) > 0);
            assert_eq!(
                core::slice::from_raw_parts(observer.as_ptr().add(slot + 32), frame.len()),
                frame
            );
        }

        forced.close().expect("forced writer closes");
        forced.close().expect("close is idempotent");
        // SAFETY: `original` keeps an independent mapped view live after forced
        // close and can observe the required STOPPING publication.
        assert_eq!(
            unsafe { core::ptr::read_volatile(addr_of!((*header).state)) },
            STATE_STOPPING
        );
        original.close().expect("original writer closes");
    }

    #[test]
    fn force_open_rejects_a_small_existing_mapping() {
        let name = test_name("small");
        // SAFETY: Creates a page-file-backed test mapping with a unique name and
        // a deliberately insufficient one-page extent.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                4096,
                name.as_ptr(),
            )
        };
        assert!(!raw.is_null());
        // SAFETY: Successful creation returned one owned mapping handle.
        let _mapping = unsafe { OwnedHandle::from_raw(raw) };
        let error = match ObsBackend::open_protocol(
            validated(64, 64),
            OpenOptions { force_open: true },
            &name,
        ) {
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
