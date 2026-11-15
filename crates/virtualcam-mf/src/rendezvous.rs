//! Producer rendezvous and the source-owned frame transport.
//!
//! One worker thread per source owns a single-instance message pipe:
//!
//! 1. overlapped `ConnectNamedPipe` (cancellable by Shutdown);
//! 2. bounded `Hello` read with a deadline — a stalled or chatty client is
//!    dropped, never pinning the worker;
//! 3. `ImpersonateNamedPipeClient` after that read, TokenUser SID compared
//!    with the expected owner assertion, `RevertToSelf` on every path;
//! 4. a fresh ring under a source-generated 128-bit nonce, with an explicit
//!    DACL (owner: map read/write only), never reusing a pre-existing object;
//! 5. `Welcome`, then a pending 1-byte read whose completion (pipe closed)
//!    detaches the producer: the stream returns to black immediately. While
//!    parked, the worker sends a `Request` whenever the consumers' selected
//!    mode differs from the producer's (the stream signals each selection).
//!
//! Source activation and `Start` never wait for any of this.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use virtualcam_protocol::mf::{Hello, Request, Welcome, WelcomeStatus, frame_event_name, pipe_name, ring_name};
use virtualcam_protocol::ring::{DEFAULT_SLOT_COUNT, ReadOutcome, RingLayout, RingReader, RingWriter};
use virtualcam_protocol::{ValidatedFormat, VideoFormat};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INVALID_HANDLE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WIN32_ERROR};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE, UnmapViewOfFile};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, INFINITE, SetEvent, WaitForMultipleObjects};
use windows_core::PCWSTR;

use crate::config::{Rendezvous as RendezvousConfig, SourceConfig};
use crate::security::{SecurityDescriptor, Sid, event_sddl, mapping_sddl, pipe_client_user, pipe_sddl, process_user};
use crate::{ModuleRef, guard};

const HANDSHAKE_TIMEOUT_MS: u32 = 2_000;

/// Owned kernel handle.
pub(crate) struct Handle(pub(crate) HANDLE);

// SAFETY: Kernel handles have no thread affinity.
unsafe impl Send for Handle {}
// SAFETY: Only waits/signals happen through shared references.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Owned handle closed once.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

/// A read/write view of a source-created ring mapping.
struct RingMapping {
    ptr: *mut u8,
    len: usize,
    _handle: Handle,
}

// SAFETY: Plain mapped memory without thread affinity.
unsafe impl Send for RingMapping {}

impl Drop for RingMapping {
    fn drop(&mut self) {
        // SAFETY: Base address returned by MapViewOfFile, unmapped once.
        let _ = unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr.cast() }) };
    }
}

struct Attached {
    // Field order is drop order: the reader's view outlives the reader.
    reader: RingReader,
    _mapping: RingMapping,
    /// The mode the producer writes.
    format: ValidatedFormat,
    frame_event: Arc<Handle>,
}

/// The currently attached producer ring and the consumers' selected mode,
/// shared by worker and stream.
pub(crate) struct TransportSlot {
    attached: Mutex<Option<Attached>>,
    selected: Mutex<Option<VideoFormat>>,
    /// Auto-reset event set on every selection; wakes the parked worker.
    selected_event: Handle,
    /// Wakes the stream on requests, state changes and transport replacement.
    pub(crate) wake: Handle,
}

impl TransportSlot {
    pub(crate) fn new() -> windows_core::Result<Self> {
        // SAFETY: Anonymous auto-reset events owned by this transport slot.
        let selected_event = Handle(unsafe { CreateEventW(None, false, false, None) }?);
        let wake = Handle(unsafe { CreateEventW(None, false, false, None) }?);
        Ok(Self {
            attached: Mutex::new(None),
            selected: Mutex::new(None),
            selected_event,
            wake,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Attached>> {
        self.attached.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs a seqlock'd read of the newest frame, if a producer is attached;
    /// `copy` also receives the mode the producer writes.
    /// The result also carries the ring generation.
    pub(crate) fn read_latest(&self, mut copy: impl FnMut(*const u8, &ValidatedFormat)) -> Option<(u64, ReadOutcome)> {
        self.lock().as_ref().map(|attached| (attached.reader.generation(), attached.reader.read_latest(|src| copy(src, &attached.format))))
    }

    /// The attached producer's newest frame as `(ring generation, sequence)`
    /// (sequence 0 before its first frame), or `None` without a producer.
    /// Every producer session gets a fresh ring whose sequences restart at 1,
    /// so a frame is identified by both.
    pub(crate) fn latest_frame(&self) -> Option<(u64, u64)> {
        self.lock().as_ref().map(|attached| (attached.reader.generation(), attached.reader.latest_sequence()))
    }

    pub(crate) fn frame_event(&self) -> Option<Arc<Handle>> {
        self.lock().as_ref().map(|attached| Arc::clone(&attached.frame_event))
    }

    pub(crate) fn wake(&self) {
        // SAFETY: Live slot-owned auto-reset event; signals coalesce.
        let _ = unsafe { SetEvent(self.wake.0) };
    }

    /// Records the mode consumers selected and wakes the worker.
    pub(crate) fn select(&self, format: VideoFormat) {
        *self.selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format);
        // SAFETY: Live event.
        let _ = unsafe { SetEvent(self.selected_event.0) };
    }

    fn selected(&self) -> Option<VideoFormat> {
        *self.selected.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn attach(&self, attached: Attached) {
        *self.lock() = Some(attached);
        self.wake();
    }

    fn detach(&self) {
        *self.lock() = None;
        self.wake();
    }
}

/// The rendezvous worker of one source.
pub(crate) struct Rendezvous {
    thread: JoinHandle<()>,
    cancel: Arc<Handle>,
}

impl Rendezvous {
    /// Starts the worker; `None` when the configuration carries no valid
    /// rendezvous (the source then stays black, fail closed).
    pub(crate) fn spawn(config: &SourceConfig, slot: Arc<TransportSlot>) -> Option<Self> {
        let rendezvous = config.rendezvous.clone()?;
        // SAFETY: Anonymous manual-reset event.
        let cancel = Arc::new(Handle(unsafe { CreateEventW(None, true, false, None) }.ok()?));
        let worker_cancel = Arc::clone(&cancel);
        let config = config.clone();
        let module = ModuleRef::new();
        let thread = std::thread::Builder::new()
            .name("virtualcam-mf-rendezvous".into())
            .spawn(move || {
                let _module = module;
                let _ = guard(|| {
                    run(&config, &rendezvous, &slot, &worker_cancel);
                    slot.detach();
                    Ok(())
                });
            })
            .ok()?;
        Some(Self { thread, cancel })
    }

    /// Cancels pending pipe I/O and joins the worker. Call without holding
    /// any source/stream lock.
    pub(crate) fn stop(self) {
        // SAFETY: Live event.
        let _ = unsafe { SetEvent(self.cancel.0) };
        let _ = self.thread.join();
    }
}

enum Io {
    Done(u32),
    Cancelled,
    Failed,
}

/// Completes one overlapped operation started by `start`, honoring `cancel`
/// and an optional timeout. The OVERLAPPED is always drained before return.
fn overlapped(pipe: HANDLE, event: &Handle, cancel: &Handle, timeout_ms: u32, start: impl FnOnce(*mut OVERLAPPED) -> windows_core::Result<()>) -> Io {
    let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
    let started = start(&mut ov);
    if let Err(error) = &started
        && error.code() != ERROR_IO_PENDING.to_hresult()
        && error.code() != ERROR_PIPE_CONNECTED.to_hresult()
    {
        return Io::Failed;
    }
    if started.as_ref().is_err_and(|error| error.code() == ERROR_PIPE_CONNECTED.to_hresult()) {
        return Io::Done(0);
    }
    let handles = [cancel.0, event.0];
    // SAFETY: Two live handles.
    let waited = unsafe { WaitForMultipleObjects(&handles, false, timeout_ms) };
    if waited != windows::Win32::Foundation::WAIT_EVENT(WAIT_OBJECT_0.0 + 1) {
        // Cancelled or timed out: cancel and drain so `ov` can be dropped.
        // SAFETY: `ov` is the pending operation on `pipe`.
        unsafe {
            let _ = CancelIoEx(pipe, Some(&ov));
            let mut ignored = 0;
            let _ = GetOverlappedResult(pipe, &ov, &mut ignored, true);
        }
        return Io::Cancelled;
    }
    let mut bytes = 0;
    // SAFETY: The operation completed; read its result.
    match unsafe { GetOverlappedResult(pipe, &ov, &mut bytes, false) } {
        Ok(()) => Io::Done(bytes),
        Err(_) => Io::Failed,
    }
}

fn cancelled(cancel: &Handle) -> bool {
    // SAFETY: Live event.
    unsafe { windows::Win32::System::Threading::WaitForSingleObject(cancel.0, 0) == WAIT_OBJECT_0 }
}

fn random_u128() -> Option<u128> {
    let mut bytes = [0_u8; 16];
    // SAFETY: Writable buffer; system-preferred RNG needs no algorithm handle.
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().ok()?;
    Some(u128::from_le_bytes(bytes))
}

fn run(config: &SourceConfig, rendezvous: &RendezvousConfig, slot: &TransportSlot, cancel: &Handle) {
    let Some(me) = process_user() else { return };
    let Some(descriptor) = pipe_sddl(&me, &rendezvous.expected_owner).and_then(|sddl| SecurityDescriptor::from_sddl(&sddl)) else {
        return;
    };
    let attributes = descriptor.attributes();
    let name = pipe_name(&rendezvous.id);
    // SAFETY: NUL-terminated name and a valid explicit security descriptor.
    // FIRST_PIPE_INSTANCE fails if anyone pre-created the name (squatting).
    let pipe = Handle(unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            Welcome::SIZE as u32,
            Hello::SIZE as u32,
            0,
            Some(&attributes),
        )
    });
    if pipe.0 == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: Anonymous manual-reset event for overlapped I/O.
    let Ok(event) = (unsafe { CreateEventW(None, true, false, None) }) else { return };
    let event = Handle(event);

    while !cancelled(cancel) {
        // SAFETY: Overlapped connect on our server pipe.
        match overlapped(pipe.0, &event, cancel, INFINITE, |ov| unsafe { ConnectNamedPipe(pipe.0, Some(ov)) }) {
            Io::Cancelled => break,
            Io::Failed => {
                // SAFETY: Reset a broken instance before listening again.
                let _ = unsafe { DisconnectNamedPipe(pipe.0) };
                continue;
            }
            Io::Done(_) => {}
        }
        serve(config, rendezvous, &me, slot, pipe.0, &event, cancel);
        slot.detach();
        // SAFETY: Drops the client so the instance can accept the next one.
        let _ = unsafe { DisconnectNamedPipe(pipe.0) };
    }
}

fn write_welcome(pipe: HANDLE, event: &Handle, cancel: &Handle, welcome: &Welcome) -> bool {
    let Ok(bytes) = welcome.encode() else { return false };
    // SAFETY: `bytes` outlives the drained overlapped write.
    matches!(overlapped(pipe, event, cancel, HANDSHAKE_TIMEOUT_MS, |ov| unsafe { WriteFile(pipe, Some(&bytes), None, Some(ov)) }), Io::Done(n) if n as usize == bytes.len())
}

fn serve(config: &SourceConfig, rendezvous: &RendezvousConfig, me: &Sid, slot: &TransportSlot, pipe: HANDLE, event: &Handle, cancel: &Handle) {
    // One spare byte: a longer message fails with ERROR_MORE_DATA.
    let mut buffer = [0_u8; Hello::SIZE + 1];
    // SAFETY: `buffer` outlives the drained overlapped read.
    let Io::Done(read) = overlapped(pipe, event, cancel, HANDSHAKE_TIMEOUT_MS, |ov| unsafe { ReadFile(pipe, Some(&mut buffer), None, Some(ov)) }) else {
        return;
    };
    let Ok(hello) = Hello::decode(&buffer[..read as usize]) else { return };
    // The producer must write one of the declared modes exactly; the source
    // never converts, so any other layout is refused up front.
    let offered = config.offer_of(&hello.format);
    // The bounded message was read, so impersonation reflects this client.
    let status = match pipe_client_user(pipe) {
        Some(user) if user.equals(&rendezvous.expected_owner) => {
            if hello.camera_id != config.camera_id {
                WelcomeStatus::CameraMismatch
            } else if offered.is_none() {
                WelcomeStatus::FormatMismatch
            } else {
                WelcomeStatus::Ok
            }
        }
        _ => WelcomeStatus::Unauthorized,
    };
    if status != WelcomeStatus::Ok {
        write_welcome(pipe, event, cancel, &Welcome::failure(status));
        return;
    }

    let Some(format) = offered else { return };
    let Some((attached, welcome)) = create_ring(&format, me, &rendezvous.expected_owner, pipe) else {
        write_welcome(pipe, event, cancel, &Welcome::failure(WelcomeStatus::Internal));
        return;
    };
    slot.attach(attached);
    if !write_welcome(pipe, event, cancel, &welcome) {
        return;
    }
    park(slot, pipe, event, cancel, hello.format);
}

/// Waits until the producer closes its end (or we are cancelled), sending a
/// `Request` whenever the selected mode differs from the producer's.
fn park(slot: &TransportSlot, pipe: HANDLE, event: &Handle, cancel: &Handle, producing: VideoFormat) {
    // SAFETY: Anonymous manual-reset event for overlapped writes.
    let Ok(write_event) = (unsafe { CreateEventW(None, true, false, None) }) else { return };
    let write_event = Handle(write_event);
    // The mode the producer writes or was last asked for.
    let mut asked = producing;
    let request = |asked: &mut VideoFormat| -> bool {
        let Some(selected) = slot.selected().filter(|selected| selected != asked) else { return true };
        let Ok(bytes) = (Request { format: selected }).encode() else { return true };
        // SAFETY: `bytes` outlives the drained overlapped write.
        let sent = matches!(overlapped(pipe, &write_event, cancel, HANDSHAKE_TIMEOUT_MS, |ov| unsafe { WriteFile(pipe, Some(&bytes), None, Some(ov)) }), Io::Done(n) if n as usize == bytes.len());
        if sent {
            *asked = selected;
        }
        sent
    };
    if !request(&mut asked) {
        return;
    }
    let mut byte = [0_u8; 1];
    loop {
        let mut ov = OVERLAPPED { hEvent: event.0, ..Default::default() };
        // SAFETY: `byte` and `ov` outlive the read, which is drained below
        // on every path that leaves this iteration while it is pending.
        match unsafe { ReadFile(pipe, Some(&mut byte), None, Some(&mut ov)) } {
            Ok(()) => continue,
            Err(error) if error.code() != ERROR_IO_PENDING.to_hresult() => return,
            Err(_) => {}
        }
        let drain = |ov: &OVERLAPPED| {
            // SAFETY: `ov` is the pending read on `pipe`.
            unsafe {
                let _ = CancelIoEx(pipe, Some(ov));
                let mut ignored = 0;
                let _ = GetOverlappedResult(pipe, ov, &mut ignored, true);
            }
        };
        loop {
            let handles = [cancel.0, event.0, slot.selected_event.0];
            // SAFETY: Three live handles.
            let waited = unsafe { WaitForMultipleObjects(&handles, false, INFINITE) };
            match waited.0.wrapping_sub(WAIT_OBJECT_0.0) {
                1 => {
                    let mut bytes = 0;
                    // SAFETY: The read completed.
                    if unsafe { GetOverlappedResult(pipe, &ov, &mut bytes, false) }.is_err() {
                        return;
                    }
                    break;
                }
                2 if !cancelled(cancel) => {
                    if !request(&mut asked) {
                        drain(&ov);
                        return;
                    }
                }
                _ => {
                    drain(&ov);
                    return;
                }
            }
        }
    }
}

/// Creates a fresh ring the authenticated owner may map read/write.
fn create_ring(format: &ValidatedFormat, me: &Sid, owner: &Sid, pipe: HANDLE) -> Option<(Attached, Welcome)> {
    let layout = RingLayout::for_format(format, DEFAULT_SLOT_COUNT).ok()?;
    let descriptor = SecurityDescriptor::from_sddl(&mapping_sddl(me, owner)?)?;
    let attributes = descriptor.attributes();
    let event_descriptor = SecurityDescriptor::from_sddl(&event_sddl(me, owner)?)?;
    let event_attributes = event_descriptor.attributes();
    let mut client_pid = 0;
    // SAFETY: Connected pipe; writable output.
    let _ = unsafe { GetNamedPipeClientProcessId(pipe, &mut client_pid) };
    let same_session = {
        let (mut client, mut own) = (u32::MAX, u32::MAX - 1);
        // SAFETY: Writable outputs.
        unsafe {
            let _ = GetNamedPipeClientSessionId(pipe, &mut client);
            let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut own);
        }
        client == own
    };
    let size = layout.total_size as u64;
    for _ in 0..4 {
        let nonce = random_u128()?;
        for global in [true, false] {
            // `Local\` is only reachable from the caller's own session; use
            // it only when Global is unavailable (no SeCreateGlobalPrivilege,
            // i.e. not hosted by Frame Server) and the client shares it.
            if !global && !same_session {
                continue;
            }
            let name = ring_name(global, nonce);
            let wide: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
            // SAFETY: Page-file section with an explicit descriptor.
            let created = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&attributes), PAGE_READWRITE, (size >> 32) as u32, size as u32, PCWSTR(wide.as_ptr())) };
            // SAFETY: Reads the thread's last error right after the call.
            let error: WIN32_ERROR = unsafe { GetLastError() };
            let handle = match created {
                Ok(handle) => Handle(handle),
                Err(_) if error == ERROR_ACCESS_DENIED && global => continue,
                Err(_) => break,
            };
            if error == ERROR_ALREADY_EXISTS || error == ERROR_INVALID_HANDLE {
                // Never reuse an object someone else created at our name.
                break;
            }
            let frame_name = frame_event_name(global, nonce);
            let frame_wide: Vec<u16> = frame_name.encode_utf16().chain(core::iter::once(0)).collect();
            let Some(frame_event) = create_fresh_event(&frame_wide, &event_attributes) else { break };
            // SAFETY: Whole-section read/write view.
            let view = unsafe { MapViewOfFile(handle.0, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
            if view.Value.is_null() {
                return None;
            }
            let mapping = RingMapping {
                ptr: view.Value.cast(),
                len: layout.total_size,
                _handle: handle,
            };
            let generation = u64::try_from(nonce >> 64).ok()?.max(1);
            // SAFETY: Fresh exclusive view; the producer attaches as writer later.
            unsafe { RingWriter::initialize(mapping.ptr, mapping.len, format, layout.slot_count, generation, client_pid) }.ok()?;
            // SAFETY: The view lives as long as the reader (stored together).
            let reader = unsafe { RingReader::open(mapping.ptr, mapping.len) }.ok()?;
            let welcome = Welcome {
                status: WelcomeStatus::Ok,
                slot_count: layout.slot_count,
                ring_size: size,
                generation,
                ring_name: name,
                frame_event_name: frame_name,
            };
            return Some((
                Attached {
                    reader,
                    _mapping: mapping,
                    format: *format,
                    frame_event: Arc::new(frame_event),
                },
                welcome,
            ));
        }
    }
    None
}

/// Refuses both same-type and different-type pre-existing named objects.
fn create_fresh_event(name: &[u16], attributes: &windows::Win32::Security::SECURITY_ATTRIBUTES) -> Option<Handle> {
    // SAFETY: NUL-terminated name and live explicit security descriptor.
    let created = unsafe { CreateEventW(Some(attributes), false, false, PCWSTR(name.as_ptr())) };
    // SAFETY: Must read immediately after creation to detect collisions.
    let error = unsafe { GetLastError() };
    let handle = Handle(created.ok()?);
    if error == ERROR_ALREADY_EXISTS || error == ERROR_INVALID_HANDLE {
        return None;
    }
    Some(handle)
}

#[cfg(test)]
mod tests {
    use virtualcam_protocol::{FrameRate, PixelFormat};
    use windows::Win32::Media::MediaFoundation::{IMFMediaEventGenerator, IMFMediaStream2, MFCreateAttributes, MFCreateEventQueue, MFCreateStreamDescriptor};
    use windows::Win32::System::Threading::{EVENT_ALL_ACCESS, EVENT_MODIFY_STATE, OpenEventW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject};
    use windows_core::{ComObject, Interface};

    use super::*;

    #[test]
    fn queued_requests_wait_for_frame_events_and_detach_wakes_them() {
        // SAFETY: Test thread joins the MTA and starts MF before creating objects.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
            windows::Win32::Media::MediaFoundation::MFStartup(windows::Win32::Media::MediaFoundation::MF_VERSION, 0).expect("MF");
        }
        let format = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30)).validate().expect("format");
        let layout = RingLayout::for_format(&format, DEFAULT_SLOT_COUNT).expect("layout");
        // SAFETY: Anonymous test objects; each returned handle/view is owned below.
        let handle = Handle(unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, layout.total_size as u32, None) }.expect("mapping"));
        let view = unsafe { MapViewOfFile(handle.0, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        assert!(!view.Value.is_null());
        let mapping = RingMapping {
            ptr: view.Value.cast(),
            len: layout.total_size,
            _handle: handle,
        };
        // SAFETY: Fresh exclusive mapping, retained by `slot` until the writer is dropped.
        let mut writer = unsafe { RingWriter::initialize(mapping.ptr, mapping.len, &format, layout.slot_count, 1, 0) }.expect("writer");
        let reader = unsafe { RingReader::open(mapping.ptr, mapping.len) }.expect("reader");
        let frame_event = Arc::new(Handle(unsafe { CreateEventW(None, false, false, None) }.expect("frame event")));
        let slot = Arc::new(TransportSlot::new().expect("slot"));
        slot.attach(Attached {
            reader,
            _mapping: mapping,
            format,
            frame_event: Arc::clone(&frame_event),
        });
        let media_type = crate::source::mode_media_type(&format).expect("media type");
        // SAFETY: Valid MF types, descriptors and queues for this test stream.
        let (queue, descriptor, attributes) = unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1).expect("attributes");
            (
                MFCreateEventQueue().expect("queue"),
                MFCreateStreamDescriptor(0, &[Some(media_type.clone())]).expect("descriptor"),
                attributes.expect("attributes"),
            )
        };
        let stream = ComObject::new(crate::stream::MediaStream::new(queue, descriptor, attributes, vec![(format, media_type)], 0, Arc::clone(&slot)));
        let interface: IMFMediaStream2 = stream.to_interface();
        let events: IMFMediaEventGenerator = interface.cast().expect("generator");
        stream.start(0, 0).expect("start");
        let _ = crate::tests::events(&events);
        // SAFETY: Writer's slot spans this tight frame. Deliberately omit notification.
        unsafe { core::ptr::write_bytes(writer.begin_frame(), 16, format.frame_size()) };
        writer.commit_frame(0);
        unsafe { interface.RequestSample(None) }.expect("first request");
        let _ = crate::tests::next_sample(&events);
        unsafe { interface.RequestSample(None) }.expect("pending request");
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe { core::ptr::write_bytes(writer.begin_frame(), 123, format.frame_size()) };
        writer.commit_frame(1);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(crate::tests::events(&events).is_empty(), "a queued request must not poll the ring");
        let notified = std::time::Instant::now();
        unsafe { SetEvent(frame_event.0) }.expect("notify");
        let sample = crate::tests::next_sample(&events);
        assert!(notified.elapsed() < std::time::Duration::from_millis(250), "new frame wakes immediately, before the one-second repeat");
        // SAFETY: Valid sample with a linear frame buffer.
        unsafe {
            let buffer = sample.GetBufferByIndex(0).expect("buffer");
            let mut data = core::ptr::null_mut();
            buffer.Lock(&mut data, None, None).expect("lock");
            assert_eq!(*data, 123, "event delivered the newest committed frame");
            buffer.Unlock().expect("unlock");
            interface.RequestSample(None).expect("pending request before detach");
        }
        let detached = std::time::Instant::now();
        slot.detach();
        let _ = crate::tests::next_sample(&events);
        assert!(detached.elapsed() < std::time::Duration::from_millis(250), "detach wakes the worker waiting on the old event");
        stream.shutdown();
    }

    #[test]
    fn frame_event_has_owner_rights_and_rejects_name_collisions() {
        let owner = process_user().expect("owner");
        let source = Sid::parse("S-1-5-19").expect("service");
        let descriptor = SecurityDescriptor::from_sddl(&event_sddl(&source, &owner).expect("sddl")).expect("descriptor");
        let name: Vec<u16> = frame_event_name(false, random_u128().expect("nonce")).encode_utf16().chain(core::iter::once(0)).collect();
        let event = create_fresh_event(&name, &descriptor.attributes()).expect("fresh event");
        // SAFETY: Live explicit descriptor, NUL-terminated name and owned handles.
        unsafe {
            let producer = Handle(OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())).expect("owner signal/wait rights"));
            assert!(OpenEventW(EVENT_ALL_ACCESS, false, PCWSTR(name.as_ptr())).is_err(), "owner is not granted full event access");
            SetEvent(producer.0).expect("signal");
            assert!(create_fresh_event(&name, &descriptor.attributes()).is_none(), "never reuse an existing event");
            assert_eq!(WaitForSingleObject(event.0, 0), WAIT_OBJECT_0, "collision did not reset the existing event");
        }
        drop(event);
        let mapping_descriptor = SecurityDescriptor::from_sddl(&mapping_sddl(&source, &owner).expect("sddl")).expect("descriptor");
        // SAFETY: Fresh page-file mapping at the former event name.
        let _mapping = Handle(unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&mapping_descriptor.attributes()), PAGE_READWRITE, 0, 4096, PCWSTR(name.as_ptr())) }.expect("mapping"));
        assert!(create_fresh_event(&name, &descriptor.attributes()).is_none(), "never reuse a different object type");
    }
}
