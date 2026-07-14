use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, GetLastError, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Memory::{FILE_MAP_WRITE, OpenFileMappingA},
        Threading::{CreateEventA, EVENT_MODIFY_STATE, OpenEventA, SetEvent, WaitForSingleObject},
        WindowsProgramming::OpenMutexA,
    },
};

use crate::{
    CameraType, Error, Result, WriteStatus,
    config::ValidatedUnityCaptureOptions,
    platform::windows::{MappedView, MutexGuard, OwnedHandle, last_error, registry_key_exists},
};

use super::protocol::unity_capture::{
    DATA_OFFSET, MAX_SHARED_IMAGE_SIZE, ObjectNames, SharedMemHeader,
};

const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

#[cfg(target_pointer_width = "64")]
const UNITY_CLSID_SUFFIX_BASE: u8 = 0x10;
#[cfg(target_pointer_width = "32")]
const UNITY_CLSID_SUFFIX_BASE: u8 = 0x20;

pub(crate) struct UnityCaptureBackend {
    view: Option<MappedView>,
    mapping: Option<OwnedHandle>,
    sent_event: Option<OwnedHandle>,
    want_event: Option<OwnedHandle>,
    mutex: Option<OwnedHandle>,
    names: ObjectNames,
    width: i32,
    height: i32,
    stride_pixels: i32,
    frame_size: u32,
    format: i32,
    resize_mode: i32,
    mirror_mode: i32,
    timeout_ms: i32,
    open: bool,
}

impl UnityCaptureBackend {
    pub(crate) fn open(options: ValidatedUnityCaptureOptions) -> Result<Self> {
        if !registry_key_exists(&unity_clsid_key(options.capture_number)) {
            return Err(Error::BackendUnavailable(CameraType::UnityCapture));
        }
        Self::open_protocol(options, ObjectNames::for_capture(options.capture_number)?)
    }

    fn open_protocol(options: ValidatedUnityCaptureOptions, names: ObjectNames) -> Result<Self> {
        let width = i32::try_from(options.width).map_err(|_| Error::InvalidDimensions)?;
        let height = i32::try_from(options.height).map_err(|_| Error::InvalidDimensions)?;
        let stride_pixels =
            i32::try_from(options.stride_pixels).map_err(|_| Error::InvalidDimensions)?;
        let frame_size = u32::try_from(options.frame_size).map_err(|_| Error::FrameSizeOverflow)?;
        if options.frame_size > MAX_SHARED_IMAGE_SIZE {
            return Err(Error::InvalidDimensions);
        }

        Ok(Self {
            view: None,
            mapping: None,
            sent_event: None,
            want_event: None,
            mutex: None,
            names,
            width,
            height,
            // Upstream divides D3D RowPitch by bytes per pixel, so stride is in
            // pixels for RGBA8 and both RGBA binary16 formats.
            stride_pixels,
            frame_size,
            format: options.format as i32,
            resize_mode: options.resize_mode as i32,
            mirror_mode: options.mirror_mode as i32,
            timeout_ms: options.timeout_ms,
            open: true,
        })
    }

    pub(crate) fn is_ready(&mut self) -> Result<bool> {
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        let ready = self.try_connect_sender()?;
        if !ready {
            trace_debug!("Unity Capture consumer is inactive");
        }
        Ok(ready)
    }

    pub(crate) fn write_host_frame(&mut self, frame: &[u8]) -> Result<WriteStatus> {
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        if !self.try_connect_sender()? {
            return Ok(WriteStatus::ConsumerInactive);
        }

        let view = self.view.as_ref().ok_or(Error::ConsumerInactive)?;
        let header = view.as_ptr().cast::<SharedMemHeader>();
        // Upstream checks maxSize before taking the mutex.
        // SAFETY: A connected view was validated to contain the shared header;
        // `max_size` is the first aligned DWORD written by the receiver.
        let maximum = unsafe { core::ptr::read(core::ptr::addr_of!((*header).max_size)) };
        if maximum < self.frame_size {
            return Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "frame exceeds receiver maxSize",
            });
        }
        let required = DATA_OFFSET
            .checked_add(frame.len())
            .ok_or(Error::FrameSizeOverflow)?;
        if view.accessible_len() < required {
            return Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "Unity Capture mapping is smaller than the configured frame",
            });
        }

        let mutex = self.mutex.as_ref().ok_or(Error::ConsumerInactive)?.raw();
        let guard = MutexGuard::acquire(mutex, "WaitForSingleObject(UnityCapture_Mutx)")?;
        // SAFETY: The mapped view is writable through FILE_MAP_WRITE and contains
        // `required` bytes. The current thread owns the protocol mutex. Each field
        // is naturally aligned and uses the exact C ABI type. `frame` is a distinct
        // tightly packed source of exactly `self.frame_size` bytes.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).width), self.width);
            core::ptr::write(core::ptr::addr_of_mut!((*header).height), self.height);
            core::ptr::write(
                core::ptr::addr_of_mut!((*header).stride),
                self.stride_pixels,
            );
            core::ptr::write(core::ptr::addr_of_mut!((*header).format), self.format);
            core::ptr::write(
                core::ptr::addr_of_mut!((*header).resize_mode),
                self.resize_mode,
            );
            core::ptr::write(
                core::ptr::addr_of_mut!((*header).mirror_mode),
                self.mirror_mode,
            );
            core::ptr::write(core::ptr::addr_of_mut!((*header).timeout), self.timeout_ms);
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                view.as_ptr().add(DATA_OFFSET),
                frame.len(),
            );
        }
        guard.release("ReleaseMutex(UnityCapture_Mutx)")?;

        let sent = self
            .sent_event
            .as_ref()
            .ok_or(Error::ConsumerInactive)?
            .raw();
        // SAFETY: `sent` is the live event opened with EVENT_MODIFY_STATE.
        if unsafe { SetEvent(sent) } == 0 {
            return Err(last_error("SetEvent(UnityCapture_Sent)"));
        }

        let want = self
            .want_event
            .as_ref()
            .ok_or(Error::ConsumerInactive)?
            .raw();
        // SAFETY: `want` is a live auto-reset event created by this sender. A zero
        // timeout implements upstream's non-blocking frame-skip test.
        let wait = unsafe { WaitForSingleObject(want, 0) };
        match wait {
            WAIT_OBJECT_0 => Ok(WriteStatus::Written),
            WAIT_TIMEOUT => {
                trace_debug!("Unity Capture consumer skipped a frame");
                Ok(WriteStatus::FrameSkippedByConsumer)
            }
            WAIT_FAILED => Err(last_error("WaitForSingleObject(UnityCapture_Want, 0)")),
            _ => Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "want-frame event returned an unexpected wait status",
            }),
        }
    }

    fn try_connect_sender(&mut self) -> Result<bool> {
        if self.view.is_some() {
            return Ok(true);
        }

        if self.mutex.is_none() {
            // SAFETY: Name is a stable NUL-terminated ANSI string. SYNCHRONIZE is
            // the exact sender access mask in `SharedImageMemory::Open(false)`.
            let raw =
                unsafe { OpenMutexA(SYNCHRONIZE_ACCESS, 0, self.names.mutex.as_ptr().cast()) };
            if raw.is_null() {
                return inactive_or_error("OpenMutexA(UnityCapture_Mutx)");
            }
            // SAFETY: Successful OpenMutexA returned one owned kernel handle.
            self.mutex = Some(unsafe { OwnedHandle::from_raw(raw) });
        }

        let mutex = self.mutex.as_ref().ok_or(Error::ConsumerInactive)?.raw();
        let guard = MutexGuard::acquire(mutex, "WaitForSingleObject(UnityCapture_Mutx/open)")?;

        if self.want_event.is_none() {
            // false manual-reset makes this an automatic-reset event, matching
            // sender-side CreateEventA in upstream Open(false).
            // SAFETY: Null security attributes and stable NUL-terminated name are
            // valid; the returned handle is checked before ownership is taken.
            let raw =
                unsafe { CreateEventA(std::ptr::null(), 0, 0, self.names.want.as_ptr().cast()) };
            if raw.is_null() {
                let error = last_error("CreateEventA(UnityCapture_Want)");
                guard.release("ReleaseMutex(UnityCapture_Mutx/open error)")?;
                return Err(error);
            }
            // SAFETY: Successful CreateEventA returned one owned event handle.
            self.want_event = Some(unsafe { OwnedHandle::from_raw(raw) });
        }

        if self.sent_event.is_none() {
            // SAFETY: Stable NUL-terminated name and exact upstream sender access.
            let raw = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, self.names.sent.as_ptr().cast()) };
            if raw.is_null() {
                // Capture the opening error before releasing the mutex changes
                // thread-local last-error state.
                // SAFETY: `GetLastError` reads thread-local state.
                let code = unsafe { GetLastError() };
                guard.release("ReleaseMutex(UnityCapture_Mutx/open inactive)")?;
                return inactive_code_or_error("OpenEventA(UnityCapture_Sent)", code);
            }
            // SAFETY: Successful OpenEventA returned one owned event handle.
            self.sent_event = Some(unsafe { OwnedHandle::from_raw(raw) });
        }

        if self.mapping.is_none() {
            // SAFETY: Stable NUL-terminated name and exact FILE_MAP_WRITE access.
            let raw =
                unsafe { OpenFileMappingA(FILE_MAP_WRITE, 0, self.names.data.as_ptr().cast()) };
            if raw.is_null() {
                // SAFETY: `GetLastError` reads thread-local state immediately.
                let code = unsafe { GetLastError() };
                guard.release("ReleaseMutex(UnityCapture_Mutx/open inactive)")?;
                return inactive_code_or_error("OpenFileMappingA(UnityCapture_Data)", code);
            }
            // SAFETY: Successful OpenFileMappingA returned one owned handle.
            self.mapping = Some(unsafe { OwnedHandle::from_raw(raw) });
        }

        if self.view.is_none() {
            let mapping = self.mapping.as_ref().ok_or(Error::ConsumerInactive)?.raw();
            let view =
                MappedView::map(mapping, FILE_MAP_WRITE, "MapViewOfFile(UnityCapture_Data)")?;
            if view.accessible_len() < DATA_OFFSET {
                guard.release("ReleaseMutex(UnityCapture_Mutx/open invalid mapping)")?;
                return Err(Error::Protocol {
                    backend: CameraType::UnityCapture,
                    detail: "Unity Capture mapping does not contain SharedMemHeader",
                });
            }
            self.view = Some(view);
        }

        guard.release("ReleaseMutex(UnityCapture_Mutx/open)")?;
        Ok(true)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        // Upstream's sender has no protocol-visible closing transition. Rust
        // additionally unmaps its owned view instead of leaking it until exit.
        self.view = None;
        self.mutex = None;
        self.want_event = None;
        self.sent_event = None;
        self.mapping = None;
        Ok(())
    }
}

fn unity_clsid_key(capture_number: u8) -> String {
    // Upstream reserves base+1 for the property page, so device #2 begins at
    // base+2 while capture zero retains the architecture-specific base CLSID.
    let suffix = if capture_number == 0 {
        UNITY_CLSID_SUFFIX_BASE
    } else {
        UNITY_CLSID_SUFFIX_BASE + 1 + capture_number
    };
    format!("CLSID\\{{5C2CD55C-92AD-4999-8666-912BD3E700{suffix:02X}}}")
}

impl Drop for UnityCaptureBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn inactive_or_error(operation: &'static str) -> Result<bool> {
    // SAFETY: Called immediately after a failed Win32 open operation.
    let code = unsafe { GetLastError() };
    inactive_code_or_error(operation, code)
}

fn inactive_code_or_error(operation: &'static str, code: u32) -> Result<bool> {
    if code == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        trace_warn!(operation, code, "Unity Capture handshake failed");
        Err(Error::Windows { operation, code })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use windows_sys::Win32::{
        Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
        System::{
            Memory::{CreateFileMappingA, FILE_MAP_WRITE, PAGE_READWRITE},
            Threading::{CreateEventA, CreateMutexA, OpenEventA, SetEvent, WaitForSingleObject},
        },
    };

    use super::*;
    use crate::backend::windows::protocol::unity_capture::{
        MAX_SHARED_IMAGE_SIZE, SharedMemHeader,
    };
    use crate::{
        UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions, UnityCaptureResizeMode,
    };

    fn test_names(suffix: &str) -> ObjectNames {
        let stem = format!(
            "Local\\VirtualCamRustUnity_{}_{}",
            std::process::id(),
            suffix
        );
        ObjectNames {
            mutex: CString::new(format!("{stem}_Mutx")).expect("mutex name"),
            want: CString::new(format!("{stem}_Want")).expect("want name"),
            sent: CString::new(format!("{stem}_Sent")).expect("sent name"),
            data: CString::new(format!("{stem}_Data")).expect("data name"),
        }
    }

    fn validated() -> ValidatedUnityCaptureOptions {
        UnityCaptureOptions {
            width: 2,
            height: 2,
            ..UnityCaptureOptions::default()
        }
        .validate()
        .expect("valid Unity Capture test configuration")
    }

    fn validated_with_controls() -> ValidatedUnityCaptureOptions {
        UnityCaptureOptions {
            width: 2,
            height: 2,
            format: UnityCaptureFormat::Rgba16FloatGamma,
            stride_pixels: Some(3),
            resize_mode: UnityCaptureResizeMode::Linear,
            mirror_mode: UnityCaptureMirrorMode::Horizontal,
            timeout_ms: 250,
            ..UnityCaptureOptions::default()
        }
        .validate()
        .expect("valid controlled Unity Capture configuration")
    }

    #[test]
    fn capture_number_maps_to_architecture_specific_clsid() {
        #[cfg(target_pointer_width = "64")]
        {
            assert!(unity_clsid_key(0).ends_with("0010}"));
            assert!(unity_clsid_key(1).ends_with("0012}"));
            assert!(unity_clsid_key(UnityCaptureOptions::MAX_CAPTURE_NUMBER).ends_with("005B}"));
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert!(unity_clsid_key(0).ends_with("0020}"));
            assert!(unity_clsid_key(1).ends_with("0022}"));
            assert!(unity_clsid_key(UnityCaptureOptions::MAX_CAPTURE_NUMBER).ends_with("006B}"));
        }
    }

    #[test]
    fn consumer_inactive_without_receiver_objects() {
        let mut sender = UnityCaptureBackend::open_protocol(
            validated(),
            test_names("inactive_without_receiver"),
        )
        .expect("construct sender state");

        assert_eq!(sender.is_ready(), Ok(false));
        assert_eq!(
            sender.write_host_frame(&[0; 16]),
            Ok(WriteStatus::ConsumerInactive)
        );
        assert!(sender.mutex.is_none());
        assert!(sender.want_event.is_none());
        sender.close().expect("close inactive sender");
        assert_eq!(sender.is_ready(), Err(Error::AlreadyClosed));
        assert_eq!(sender.write_host_frame(&[0; 16]), Err(Error::AlreadyClosed));
    }

    #[test]
    fn partial_handshake_resources_are_released_on_close() {
        let receiver_names = test_names("partial_cleanup");
        let sender_names = test_names("partial_cleanup");
        // SAFETY: Null security attributes and a stable NUL-terminated test name.
        let mutex_raw =
            unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let _mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender = UnityCaptureBackend::open_protocol(validated(), sender_names)
            .expect("construct sender state");
        assert_eq!(sender.is_ready(), Ok(false));
        assert!(sender.mutex.is_some());
        assert!(sender.want_event.is_some());
        assert!(sender.sent_event.is_none());
        assert!(sender.mapping.is_none());

        // Prove the sender created the partial-handshake Want object.
        // SAFETY: Stable NUL-terminated name and exact modify access.
        let want_raw =
            unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        drop(unsafe { OwnedHandle::from_raw(want_raw) });

        sender.close().expect("close partial sender");
        // No handle should keep the sender-created event alive after close.
        // SAFETY: A failed named-object lookup returns null and sets last error.
        let reopened =
            unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(reopened.is_null());
        // SAFETY: Read immediately after the failed OpenEventA call.
        assert_eq!(unsafe { GetLastError() }, ERROR_FILE_NOT_FOUND);
    }

    #[test]
    fn real_objects_cover_handshake_events_metadata_and_frame_skip() {
        let receiver_names = test_names("active");
        let sender_names = test_names("active");

        // Receiver Open(true) creates the mutex first.
        // SAFETY: Null security attributes and stable NUL-terminated test name.
        let mutex_raw =
            unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender =
            UnityCaptureBackend::open_protocol(validated_with_controls(), sender_names)
                .expect("construct sender state");
        assert!(!sender.is_ready().expect("first handshake attempt"));

        let receiver_guard =
            MutexGuard::acquire(mutex.raw(), "WaitForSingleObject(test receiver mutex)")
                .expect("receiver owns mutex");
        // Sender's first attempt created the automatic-reset want event.
        // SAFETY: Stable NUL-terminated name and exact receiver modify access.
        let want_raw =
            unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        let want = unsafe { OwnedHandle::from_raw(want_raw) };

        // Receiver creates the automatic-reset sent event.
        // SAFETY: Null security attributes, false reset/initial flags, stable name.
        let sent_raw =
            unsafe { CreateEventA(std::ptr::null(), 0, 0, receiver_names.sent.as_ptr().cast()) };
        assert!(!sent_raw.is_null());
        // SAFETY: Successful CreateEventA returned one owned handle.
        let sent = unsafe { OwnedHandle::from_raw(sent_raw) };

        let mapping_size = core::mem::size_of::<SharedMemHeader>()
            .checked_add(MAX_SHARED_IMAGE_SIZE)
            .expect("mapping size");
        // SAFETY: Page-file mapping size follows receiver Open(true), with stable name.
        let mapping_raw = unsafe {
            CreateFileMappingA(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                u32::try_from(mapping_size).expect("mapping fits DWORD"),
                receiver_names.data.as_ptr().cast(),
            )
        };
        assert!(!mapping_raw.is_null());
        // SAFETY: Successful CreateFileMappingA returned one owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(mapping_raw) };
        let receiver_view = MappedView::map(
            mapping.raw(),
            FILE_MAP_WRITE,
            "MapViewOfFile(test Unity receiver)",
        )
        .expect("map receiver data");
        let header = receiver_view.as_ptr().cast::<SharedMemHeader>();
        // SAFETY: Receiver view contains the aligned header and owns the mutex.
        unsafe {
            core::ptr::write(
                core::ptr::addr_of_mut!((*header).max_size),
                u32::try_from(MAX_SHARED_IMAGE_SIZE).expect("maximum fits DWORD"),
            );
        }
        receiver_guard
            .release("ReleaseMutex(test receiver open)")
            .expect("release receiver mutex");

        assert!(sender.is_ready().expect("completed sender handshake"));

        // Receiver requests a frame before waiting for Sent.
        // SAFETY: Receiver opened want with EVENT_MODIFY_STATE.
        assert_ne!(unsafe { SetEvent(want.raw()) }, 0);
        let frame = [7_u8; 48];
        assert_eq!(sender.write_host_frame(&frame), Ok(WriteStatus::Written));
        // SAFETY: Receiver owns a live waitable sent-event handle.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_OBJECT_0);

        // SAFETY: The receiver view is live and the sender completed its mutex-
        // protected metadata/data copy before signaling Sent.
        unsafe {
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).width)), 2);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).height)), 2);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).stride)), 3);
            assert_eq!(
                core::ptr::read(core::ptr::addr_of!((*header).format)),
                UnityCaptureFormat::Rgba16FloatGamma as i32
            );
            assert_eq!(
                core::ptr::read(core::ptr::addr_of!((*header).resize_mode)),
                UnityCaptureResizeMode::Linear as i32
            );
            assert_eq!(
                core::ptr::read(core::ptr::addr_of!((*header).mirror_mode)),
                UnityCaptureMirrorMode::Horizontal as i32
            );
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).timeout)), 250);
            assert_eq!(
                core::slice::from_raw_parts(receiver_view.as_ptr().add(DATA_OFFSET), frame.len()),
                frame
            );
        }

        assert_eq!(
            sender.write_host_frame(&frame),
            Ok(WriteStatus::FrameSkippedByConsumer)
        );
        // FrameSkippedByConsumer describes the absent Want request; the frame
        // data is still copied and Sent is still signaled by the protocol.
        // SAFETY: Receiver owns a live waitable Sent event handle.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_OBJECT_0);

        // A receiver capacity regression is rejected without signaling Sent.
        // SAFETY: Receiver view contains the aligned header and stays mapped.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).max_size), 47);
        }
        // SAFETY: Receiver opened want with EVENT_MODIFY_STATE.
        assert_ne!(unsafe { SetEvent(want.raw()) }, 0);
        assert!(matches!(
            sender.write_host_frame(&frame),
            Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "frame exceeds receiver maxSize",
            })
        ));
        // SAFETY: Sent is live; the prior successful signal was already consumed.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_TIMEOUT);
        sender.close().expect("sender close");
        sender.close().expect("sender close is idempotent");
    }

    #[test]
    fn connected_sender_rejects_mapping_smaller_than_frame() {
        let receiver_names = test_names("small_mapping");
        let sender_names = test_names("small_mapping");
        let config = UnityCaptureOptions {
            width: 64,
            height: 64,
            ..UnityCaptureOptions::default()
        }
        .validate()
        .expect("valid Unity Capture test configuration");

        // SAFETY: Null security attributes and stable NUL-terminated test name.
        let mutex_raw =
            unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender = UnityCaptureBackend::open_protocol(config, sender_names)
            .expect("construct sender state");
        assert_eq!(sender.is_ready(), Ok(false));
        let receiver_guard =
            MutexGuard::acquire(mutex.raw(), "WaitForSingleObject(test small mutex)")
                .expect("receiver owns mutex");

        // SAFETY: The sender's first connection attempt created this named event.
        let want_raw =
            unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        let want = unsafe { OwnedHandle::from_raw(want_raw) };
        // SAFETY: Null attributes, auto reset, initially unsignaled, stable name.
        let sent_raw =
            unsafe { CreateEventA(std::ptr::null(), 0, 0, receiver_names.sent.as_ptr().cast()) };
        assert!(!sent_raw.is_null());
        // SAFETY: Successful CreateEventA returned one owned handle.
        let sent = unsafe { OwnedHandle::from_raw(sent_raw) };

        // A 4 KiB view can hold the header but not the 16 KiB RGBA frame.
        // SAFETY: Page-file-backed mapping with a stable test-only name.
        let mapping_raw = unsafe {
            CreateFileMappingA(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                4096,
                receiver_names.data.as_ptr().cast(),
            )
        };
        assert!(!mapping_raw.is_null());
        // SAFETY: Successful CreateFileMappingA returned one owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(mapping_raw) };
        let receiver_view = MappedView::map(
            mapping.raw(),
            FILE_MAP_WRITE,
            "MapViewOfFile(test small Unity receiver)",
        )
        .expect("map small receiver data");
        let header = receiver_view.as_ptr().cast::<SharedMemHeader>();
        // SAFETY: The view contains the aligned max_size field under receiver lock.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).max_size), u32::MAX);
        }
        receiver_guard
            .release("ReleaseMutex(test small receiver open)")
            .expect("release receiver mutex");

        assert_eq!(sender.is_ready(), Ok(true));
        // SAFETY: Receiver owns a live Want event handle.
        assert_ne!(unsafe { SetEvent(want.raw()) }, 0);
        assert!(matches!(
            sender.write_host_frame(&vec![0; 64 * 64 * 4]),
            Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "Unity Capture mapping is smaller than the configured frame",
            })
        ));
        // SAFETY: Validation failed before the sender could signal Sent.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_TIMEOUT);
    }
}
