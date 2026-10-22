use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, OpenFileMappingA};
use windows_sys::Win32::System::Threading::{CreateEventA, EVENT_MODIFY_STATE, INFINITE, OpenEventA, ReleaseMutex, SetEvent, WaitForSingleObject};
use windows_sys::Win32::System::WindowsProgramming::OpenMutexA;

use super::FrameSource;
use super::protocol::unity_capture::{DATA_OFFSET, MAX_SHARED_IMAGE_SIZE, ObjectNames, SharedMemHeader};
use crate::config::ValidatedUnityCaptureOptions;
use crate::platform::windows::{MappedView, OwnedHandle, last_error};
use crate::{CameraType, Error, Result, WriteStatus};

const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

#[cfg(target_pointer_width = "64")]
const UNITY_CLSID_SUFFIX_BASE: u8 = 0x10;
#[cfg(target_pointer_width = "32")]
const UNITY_CLSID_SUFFIX_BASE: u8 = 0x20;

/// Receiver-created objects the sender has attached to so far.
///
/// Like upstream `SharedImageMemory`, the sender keeps partially opened
/// handles across attempts and, once the view exists, stays attached: its own
/// handles keep the named objects alive, so a restarted receiver reopens the
/// same objects.
#[derive(Default)]
struct Connection {
    // The view is declared first so it is unmapped before its mapping closes.
    view: Option<MappedView>,
    mapping: Option<OwnedHandle>,
    sent_event: Option<OwnedHandle>,
    want_event: Option<OwnedHandle>,
    mutex: Option<OwnedHandle>,
}

pub(crate) struct UnityCaptureBackend {
    connection: Connection,
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
    pub(crate) fn open(options: ValidatedUnityCaptureOptions, names: ObjectNames) -> Result<Self> {
        let width = i32::try_from(options.width).map_err(|_| Error::InvalidDimensions)?;
        let height = i32::try_from(options.height).map_err(|_| Error::InvalidDimensions)?;
        let stride_pixels = i32::try_from(options.stride_pixels).map_err(|_| Error::InvalidDimensions)?;
        let frame_size = u32::try_from(options.frame_size).map_err(|_| Error::FrameSizeOverflow)?;
        if options.frame_size > MAX_SHARED_IMAGE_SIZE {
            return Err(Error::InvalidDimensions);
        }

        Ok(Self {
            connection: Connection::default(),
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

    pub(super) fn write_frame(&mut self, frame: FrameSource<'_>) -> Result<WriteStatus> {
        let byte_len = frame.byte_len();
        if byte_len != self.frame_size as usize {
            return Err(Error::FrameSizeMismatch {
                expected: self.frame_size as usize,
                actual: byte_len,
            });
        }
        if !self.open {
            return Err(Error::AlreadyClosed);
        }
        if !self.try_connect_sender()? {
            return Ok(WriteStatus::ConsumerInactive);
        }

        let connection = &self.connection;
        let (Some(view), Some(mutex), Some(sent), Some(want)) = (&connection.view, &connection.mutex, &connection.sent_event, &connection.want_event) else {
            return Ok(WriteStatus::ConsumerInactive);
        };
        let header = view.as_ptr().cast::<SharedMemHeader>();
        // Upstream checks maxSize before taking the mutex.
        // SAFETY: A connected view was validated to contain the aligned shared
        // header; `max_size` is the first DWORD written by the receiver.
        let maximum = unsafe { core::ptr::read(core::ptr::addr_of!((*header).max_size)) };
        if maximum < self.frame_size {
            return Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "frame exceeds receiver maxSize",
            });
        }
        let required = DATA_OFFSET.checked_add(self.frame_size as usize).ok_or(Error::FrameSizeOverflow)?;
        if view.accessible_len() < required {
            return Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "Unity Capture mapping is smaller than the configured frame",
            });
        }

        let guard = MutexGuard::acquire(mutex.raw(), "WaitForSingleObject(UnityCapture_Mutx)")?;
        // SAFETY: The mapped view is writable through FILE_MAP_WRITE and contains
        // `required` bytes. The current thread owns the protocol mutex. Each field
        // is naturally aligned and uses the exact C ABI type.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).width), self.width);
            core::ptr::write(core::ptr::addr_of_mut!((*header).height), self.height);
            core::ptr::write(core::ptr::addr_of_mut!((*header).stride), self.stride_pixels);
            core::ptr::write(core::ptr::addr_of_mut!((*header).format), self.format);
            core::ptr::write(core::ptr::addr_of_mut!((*header).resize_mode), self.resize_mode);
            core::ptr::write(core::ptr::addr_of_mut!((*header).mirror_mode), self.mirror_mode);
            core::ptr::write(core::ptr::addr_of_mut!((*header).timeout), self.timeout_ms);
        }
        // SAFETY: `required` validation proves the full data range is writable,
        // and `guard` keeps the receiver out until the copy has completed.
        let destination = unsafe { view.as_ptr().add(DATA_OFFSET) };
        let copied = frame.copy_to(destination);
        // Release once even when the copy failed, reporting the copy's error first.
        let released = guard.release("ReleaseMutex(UnityCapture_Mutx)");
        copied?;
        released?;

        // SAFETY: `sent` is the live event opened with EVENT_MODIFY_STATE.
        if unsafe { SetEvent(sent.raw()) } == 0 {
            return Err(last_error("SetEvent(UnityCapture_Sent)"));
        }

        // SAFETY: `want` is a live auto-reset event created by this sender. A zero
        // timeout implements upstream's non-blocking frame-skip test.
        match unsafe { WaitForSingleObject(want.raw(), 0) } {
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

    /// Mirrors upstream `SharedImageMemory::Open(false)`.
    fn try_connect_sender(&mut self) -> Result<bool> {
        if self.connection.view.is_some() {
            return Ok(true);
        }

        if self.connection.mutex.is_none() {
            // SAFETY: Name is a stable NUL-terminated ANSI string. SYNCHRONIZE is
            // the exact sender access mask in `SharedImageMemory::Open(false)`.
            let raw = unsafe { OpenMutexA(SYNCHRONIZE_ACCESS, 0, self.names.mutex.as_ptr().cast()) };
            if raw.is_null() {
                // SAFETY: Read immediately after the failed OpenMutexA call.
                let code = unsafe { GetLastError() };
                return missing_object_or_error("OpenMutexA(UnityCapture_Mutx)", code);
            }
            // SAFETY: Successful OpenMutexA returned one owned kernel handle.
            self.connection.mutex = Some(unsafe { OwnedHandle::from_raw(raw) });
        }

        let Some(mutex) = self.connection.mutex.as_ref().map(OwnedHandle::raw) else {
            return Ok(false);
        };
        let guard = MutexGuard::acquire(mutex, "WaitForSingleObject(UnityCapture_Mutx/open)")?;
        // Every step runs under the receiver mutex. The guard is released once
        // afterwards so a release failure never masks the step's own error.
        let connected = attach_under_mutex(&mut self.connection, &self.names);
        let released = guard.release("ReleaseMutex(UnityCapture_Mutx/open)");
        let connected = connected?;
        released?;
        Ok(connected)
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        // Upstream's sender has no protocol-visible closing transition. Rust
        // additionally unmaps its owned view instead of leaking it until exit.
        self.connection = Connection::default();
        Ok(())
    }
}

/// The receiver-mutex-protected part of upstream `Open(false)`.
fn attach_under_mutex(connection: &mut Connection, names: &ObjectNames) -> Result<bool> {
    if connection.want_event.is_none() {
        // false manual-reset makes this an automatic-reset event, matching
        // sender-side CreateEventA in upstream Open(false).
        // SAFETY: Null security attributes and stable NUL-terminated name.
        let raw = unsafe { CreateEventA(std::ptr::null(), 0, 0, names.want.as_ptr().cast()) };
        // SAFETY: A non-null CreateEventA result is one owned event handle.
        connection.want_event = Some(unsafe { OwnedHandle::from_result(raw, "CreateEventA(UnityCapture_Want)") }?);
    }

    if connection.sent_event.is_none() {
        // SAFETY: Stable NUL-terminated name and exact upstream sender access.
        let raw = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, names.sent.as_ptr().cast()) };
        if raw.is_null() {
            // SAFETY: Read immediately after the failed OpenEventA call.
            let code = unsafe { GetLastError() };
            return missing_object_or_error("OpenEventA(UnityCapture_Sent)", code);
        }
        // SAFETY: Successful OpenEventA returned one owned event handle.
        connection.sent_event = Some(unsafe { OwnedHandle::from_raw(raw) });
    }

    let mapping = match &connection.mapping {
        Some(mapping) => mapping,
        None => {
            // SAFETY: Stable NUL-terminated name and exact FILE_MAP_WRITE access.
            let raw = unsafe { OpenFileMappingA(FILE_MAP_WRITE, 0, names.data.as_ptr().cast()) };
            if raw.is_null() {
                // SAFETY: Read immediately after the failed OpenFileMappingA call.
                let code = unsafe { GetLastError() };
                return missing_object_or_error("OpenFileMappingA(UnityCapture_Data)", code);
            }
            // SAFETY: Successful OpenFileMappingA returned one owned handle.
            connection.mapping.insert(unsafe { OwnedHandle::from_raw(raw) })
        }
    };

    let view = MappedView::map(mapping.raw(), FILE_MAP_WRITE, "MapViewOfFile(UnityCapture_Data)")?;
    if view.accessible_len() < DATA_OFFSET || view.ptr_at::<SharedMemHeader>(0).is_none() {
        return Err(Error::Protocol {
            backend: CameraType::UnityCapture,
            detail: "Unity Capture mapping does not contain SharedMemHeader",
        });
    }
    connection.view = Some(view);
    Ok(true)
}

pub(super) fn unity_clsid_key(capture_number: u8) -> String {
    // Upstream reserves base+1 for the property page, so device #2 begins at
    // base+2 while capture zero retains the architecture-specific base CLSID.
    let suffix = if capture_number == 0 { UNITY_CLSID_SUFFIX_BASE } else { UNITY_CLSID_SUFFIX_BASE + 1 + capture_number };
    format!("CLSID\\{{5C2CD55C-92AD-4999-8666-912BD3E700{suffix:02X}}}")
}

fn missing_object_or_error(operation: &'static str, code: u32) -> Result<bool> {
    if code == ERROR_FILE_NOT_FOUND {
        Ok(false)
    } else {
        trace_warn!(operation, code, "Unity Capture handshake failed");
        Err(Error::Windows { operation, code })
    }
}

/// Ownership of the Unity Capture protocol mutex by the current thread.
///
/// An abandoned mutex is treated as acquired, like upstream, which ignores the
/// wait result. Ownership never outlives one sender call.
pub(super) struct MutexGuard {
    handle: HANDLE,
    acquired: bool,
}

impl MutexGuard {
    pub(super) fn acquire(handle: HANDLE, operation: &'static str) -> Result<Self> {
        // SAFETY: `handle` is a live mutex handle with synchronization access.
        let result = unsafe { WaitForSingleObject(handle, INFINITE) };
        match result {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle, acquired: true }),
            WAIT_FAILED => Err(last_error(operation)),
            _ => Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "infinite mutex wait returned an unexpected status",
            }),
        }
    }

    pub(super) fn release(mut self, operation: &'static str) -> Result<()> {
        // Mark released only after the system confirms ownership was relinquished.
        // SAFETY: This guard was constructed only after a successful or abandoned
        // mutex wait, both of which grant ownership to the current thread.
        let released = unsafe { ReleaseMutex(self.handle) };
        if released == 0 {
            return Err(last_error(operation));
        }
        self.acquired = false;
        Ok(())
    }
}

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if self.acquired {
            // SAFETY: A live guard represents mutex ownership on this thread.
            // Drop is best-effort because it cannot return an error.
            unsafe {
                ReleaseMutex(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Memory::{CreateFileMappingA, FILE_MAP_WRITE, PAGE_READWRITE};
    use windows_sys::Win32::System::Threading::{CreateEventA, CreateMutexA, OpenEventA, SetEvent, WaitForSingleObject};

    use super::*;
    use crate::backend::windows::protocol::unity_capture::{MAX_SHARED_IMAGE_SIZE, SharedMemHeader};
    use crate::{UnityCaptureFormat, UnityCaptureMirrorMode, UnityCaptureOptions, UnityCaptureResizeMode};

    fn test_names(suffix: &str) -> ObjectNames {
        let stem = format!("Local\\VirtualCamRustUnity_{}_{}", std::process::id(), suffix);
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
        let mut sender = UnityCaptureBackend::open(validated(), test_names("inactive_without_receiver")).expect("construct sender state");

        assert_eq!(sender.is_ready(), Ok(false));
        assert_eq!(sender.write_frame(FrameSource::Host(&[0; 16])), Ok(WriteStatus::ConsumerInactive));
        assert!(sender.connection.mutex.is_none());
        assert!(sender.connection.want_event.is_none());
        sender.close().expect("close inactive sender");
        assert_eq!(sender.is_ready(), Err(Error::AlreadyClosed));
        assert_eq!(sender.write_frame(FrameSource::Host(&[0; 16])), Err(Error::AlreadyClosed));
    }

    #[test]
    fn partial_handshake_resources_are_released_on_close() {
        let receiver_names = test_names("partial_cleanup");
        let sender_names = test_names("partial_cleanup");
        // SAFETY: Null security attributes and a stable NUL-terminated test name.
        let mutex_raw = unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let _mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender = UnityCaptureBackend::open(validated(), sender_names).expect("construct sender state");
        assert_eq!(sender.is_ready(), Ok(false));
        assert!(sender.connection.mutex.is_some());
        assert!(sender.connection.want_event.is_some());
        assert!(sender.connection.sent_event.is_none());
        assert!(sender.connection.mapping.is_none());

        // Prove the sender created the partial-handshake Want object.
        // SAFETY: Stable NUL-terminated name and exact modify access.
        let want_raw = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        drop(unsafe { OwnedHandle::from_raw(want_raw) });

        sender.close().expect("close partial sender");
        // No handle should keep the sender-created event alive after close.
        // SAFETY: A failed named-object lookup returns null and sets last error.
        let reopened = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
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
        let mutex_raw = unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender = UnityCaptureBackend::open(validated_with_controls(), sender_names).expect("construct sender state");
        assert!(!sender.is_ready().expect("first handshake attempt"));

        let receiver_guard = MutexGuard::acquire(mutex.raw(), "WaitForSingleObject(test receiver mutex)").expect("receiver owns mutex");
        // Sender's first attempt created the automatic-reset want event.
        // SAFETY: Stable NUL-terminated name and exact receiver modify access.
        let want_raw = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        let want = unsafe { OwnedHandle::from_raw(want_raw) };

        // Receiver creates the automatic-reset sent event.
        // SAFETY: Null security attributes, false reset/initial flags, stable name.
        let sent_raw = unsafe { CreateEventA(std::ptr::null(), 0, 0, receiver_names.sent.as_ptr().cast()) };
        assert!(!sent_raw.is_null());
        // SAFETY: Successful CreateEventA returned one owned handle.
        let sent = unsafe { OwnedHandle::from_raw(sent_raw) };

        let mapping_size = core::mem::size_of::<SharedMemHeader>().checked_add(MAX_SHARED_IMAGE_SIZE).expect("mapping size");
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
        let receiver_view = MappedView::map(mapping.raw(), FILE_MAP_WRITE, "MapViewOfFile(test Unity receiver)").expect("map receiver data");
        let header = receiver_view.as_ptr().cast::<SharedMemHeader>();
        // SAFETY: Receiver view contains the aligned header and owns the mutex.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).max_size), u32::try_from(MAX_SHARED_IMAGE_SIZE).expect("maximum fits DWORD"));
        }
        receiver_guard.release("ReleaseMutex(test receiver open)").expect("release receiver mutex");

        assert!(sender.is_ready().expect("completed sender handshake"));

        // Receiver requests a frame before waiting for Sent.
        // SAFETY: Receiver opened want with EVENT_MODIFY_STATE.
        assert_ne!(unsafe { SetEvent(want.raw()) }, 0);
        let frame = [7_u8; 48];
        assert_eq!(sender.write_frame(FrameSource::Host(&frame)), Ok(WriteStatus::Written));
        // SAFETY: Receiver owns a live waitable sent-event handle.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_OBJECT_0);

        // SAFETY: The receiver view is live and the sender completed its mutex-
        // protected metadata/data copy before signaling Sent.
        unsafe {
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).width)), 2);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).height)), 2);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).stride)), 3);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).format)), UnityCaptureFormat::Rgba16FloatGamma as i32);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).resize_mode)), UnityCaptureResizeMode::Linear as i32);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).mirror_mode)), UnityCaptureMirrorMode::Horizontal as i32);
            assert_eq!(core::ptr::read(core::ptr::addr_of!((*header).timeout)), 250);
            assert_eq!(core::slice::from_raw_parts(receiver_view.as_ptr().add(DATA_OFFSET), frame.len()), frame);
        }

        assert_eq!(sender.write_frame(FrameSource::Host(&frame)), Ok(WriteStatus::FrameSkippedByConsumer));
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
            sender.write_frame(FrameSource::Host(&frame)),
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
        let mutex_raw = unsafe { CreateMutexA(std::ptr::null(), 0, receiver_names.mutex.as_ptr().cast()) };
        assert!(!mutex_raw.is_null());
        // SAFETY: Successful CreateMutexA returned one owned handle.
        let mutex = unsafe { OwnedHandle::from_raw(mutex_raw) };

        let mut sender = UnityCaptureBackend::open(config, sender_names).expect("construct sender state");
        assert_eq!(sender.is_ready(), Ok(false));
        let receiver_guard = MutexGuard::acquire(mutex.raw(), "WaitForSingleObject(test small mutex)").expect("receiver owns mutex");

        // SAFETY: The sender's first connection attempt created this named event.
        let want_raw = unsafe { OpenEventA(EVENT_MODIFY_STATE, 0, receiver_names.want.as_ptr().cast()) };
        assert!(!want_raw.is_null());
        // SAFETY: Successful OpenEventA returned one owned handle.
        let want = unsafe { OwnedHandle::from_raw(want_raw) };
        // SAFETY: Null attributes, auto reset, initially unsignaled, stable name.
        let sent_raw = unsafe { CreateEventA(std::ptr::null(), 0, 0, receiver_names.sent.as_ptr().cast()) };
        assert!(!sent_raw.is_null());
        // SAFETY: Successful CreateEventA returned one owned handle.
        let sent = unsafe { OwnedHandle::from_raw(sent_raw) };

        // A 4 KiB view can hold the header but not the 16 KiB RGBA frame.
        // SAFETY: Page-file-backed mapping with a stable test-only name.
        let mapping_raw = unsafe { CreateFileMappingA(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, 4096, receiver_names.data.as_ptr().cast()) };
        assert!(!mapping_raw.is_null());
        // SAFETY: Successful CreateFileMappingA returned one owned handle.
        let mapping = unsafe { OwnedHandle::from_raw(mapping_raw) };
        let receiver_view = MappedView::map(mapping.raw(), FILE_MAP_WRITE, "MapViewOfFile(test small Unity receiver)").expect("map small receiver data");
        let header = receiver_view.as_ptr().cast::<SharedMemHeader>();
        // SAFETY: The view contains the aligned max_size field under receiver lock.
        unsafe {
            core::ptr::write(core::ptr::addr_of_mut!((*header).max_size), u32::MAX);
        }
        receiver_guard.release("ReleaseMutex(test small receiver open)").expect("release receiver mutex");

        assert_eq!(sender.is_ready(), Ok(true));
        // SAFETY: Receiver owns a live Want event handle.
        assert_ne!(unsafe { SetEvent(want.raw()) }, 0);
        assert!(matches!(
            sender.write_frame(FrameSource::Host(&vec![0; 64 * 64 * 4])),
            Err(Error::Protocol {
                backend: CameraType::UnityCapture,
                detail: "Unity Capture mapping is smaller than the configured frame",
            })
        ));
        // SAFETY: Validation failed before the sender could signal Sent.
        assert_eq!(unsafe { WaitForSingleObject(sent.raw(), 0) }, WAIT_TIMEOUT);
    }
}
