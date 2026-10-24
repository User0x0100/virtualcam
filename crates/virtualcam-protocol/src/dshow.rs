//! DirectShow control plane (transport v2).
//!
//! The producer owns the runtime format and every named object, all in the
//! session-local namespace:
//!
//! ```text
//! Local\VirtualCamRs2-{ID}-Lease               event; existence = single-writer lease
//! Local\VirtualCamRs2-{ID}-Control             fixed 128-byte DshowControl mapping
//! Local\VirtualCamRs2-{ID}-Ring-{GEN:016X}     per-session frame ring
//! Local\VirtualCamRs2-{ID}-Frame-{GEN:016X}    per-session auto-reset event,
//!                                              set after every published frame
//! Local\VirtualCamRs2-{ID}-Consumer            event; existence = single-consumer lease
//! Local\VirtualCamRs2-{ID}-Request             fixed 64-byte DshowRequest mapping
//! ```
//!
//! Consumers never open the lease, so they can never make a producer's
//! occupancy check fail. Each producer session picks a fresh generation and
//! therefore a fresh ring name: a graph still holding an old ring never
//! constrains the next session's format or capacity.
//!
//! One consumer graph at a time holds the consumer lease. It is the only
//! waiter on the frame event (so an auto-reset event wakes exactly it) and
//! the only writer of the request record, through which it tells the
//! producer which pixel format its graph connected with.

use core::mem::{align_of, offset_of, size_of};
use core::ptr::{addr_of, addr_of_mut};

use crate::format::WireVideoFormat;
use crate::id::CameraId;
use crate::sync;

/// DirectShow GUIDs used by registration and the source (as `u128`).
pub mod guids {
    /// `CLSID_VideoInputDeviceCategory`.
    pub const VIDEO_INPUT_DEVICE_CATEGORY: u128 = 0x860BB310_5D01_11D0_BD3B_00A0C911CE86;
    /// `CLSID_FilterMapper2`.
    pub const FILTER_MAPPER2: u128 = 0xCDA42200_BD88_11D0_BD4E_00A0C911CE86;
    /// `CLSID_MemoryAllocator`.
    pub const MEMORY_ALLOCATOR: u128 = 0x1E651CC0_B199_11D0_8212_00C04FC32C45;
    /// `MEDIATYPE_Video`.
    pub const MEDIATYPE_VIDEO: u128 = 0x73646976_0000_0010_8000_00AA00389B71;
    /// `FORMAT_VideoInfo`.
    pub const FORMAT_VIDEO_INFO: u128 = 0x05589F80_C356_11CE_BF01_00AA0055595A;
    /// `FORMAT_VideoInfo2`.
    pub const FORMAT_VIDEO_INFO2: u128 = 0xF72A76A0_EB0A_11D0_ACE4_0000C0CC16BA;
    /// `AMPROPSETID_Pin`.
    pub const AMPROPSETID_PIN: u128 = 0x9B00F101_1567_11D1_B3F1_00AA003761C5;
    /// `PIN_CATEGORY_CAPTURE`.
    pub const PIN_CATEGORY_CAPTURE: u128 = 0xFB6C4281_0353_11D1_905F_0000C0CC16BA;
    /// `IID_IAMFilterData`.
    pub const IID_IAM_FILTER_DATA: u128 = 0x97F7C4D4_547B_4A5F_8332_536430AD2E4D;
}

/// Name of the source's single output pin.
pub const PIN_NAME: &str = "Capture";

/// `DshowControl::magic`: `"VCD2"`.
pub const CONTROL_MAGIC: u32 = u32::from_le_bytes(*b"VCD2");
/// Current control layout version.
pub const CONTROL_VERSION: u32 = 1;
/// No producer session has been published.
pub const CONTROL_STATE_NONE: u32 = 0;
/// A producer session is published and its ring exists.
pub const CONTROL_STATE_ACTIVE: u32 = 1;
/// The last producer session closed gracefully.
pub const CONTROL_STATE_STOPPED: u32 = 2;

/// Fixed control mapping, guarded by its own sequence lock.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DshowControl {
    /// [`CONTROL_MAGIC`].
    pub magic: u32,
    /// [`CONTROL_VERSION`].
    pub version: u32,
    /// `CONTROL_STATE_*`.
    pub state: u32,
    /// Sequence lock: odd while the producer rewrites the record.
    pub sequence: u32,
    /// Session generation; names the ring.
    pub generation: u64,
    /// Producer process id.
    pub producer_pid: u32,
    /// Ring slot count.
    pub slot_count: u32,
    /// Producer process creation time (`FILETIME` as `u64`), so consumers
    /// can reject a recycled process id.
    pub producer_start_time: u64,
    /// Ring mapping size in bytes.
    pub ring_size: u64,
    /// Session format.
    pub format: WireVideoFormat,
    /// Must be zero.
    pub reserved: [u64; 6],
}

const _: () = {
    assert!(size_of::<DshowControl>() == 128);
    assert!(align_of::<DshowControl>() == 64);
    assert!(offset_of!(DshowControl, sequence) == 12);
    assert!(offset_of!(DshowControl, generation) == 16);
    assert!(offset_of!(DshowControl, producer_start_time) == 32);
    assert!(offset_of!(DshowControl, format) == 48);
};

impl DshowControl {
    /// Byte size of the control mapping.
    pub const SIZE: usize = size_of::<Self>();

    /// Publishes `record` (state included) under the sequence lock.
    ///
    /// # Safety
    ///
    /// `control` must point to a live, aligned, writable control mapping with
    /// a single writer (the lease holder).
    pub unsafe fn publish(control: *mut Self, record: &Self) {
        // SAFETY: Forwarded caller contract.
        unsafe {
            let sequence = sync::observe_u32(addr_of!((*control).sequence));
            let odd = sequence.wrapping_add(1) | 1;
            sync::publish_u32(addr_of_mut!((*control).sequence), odd);
            let mut copy = *record;
            copy.sequence = odd;
            core::ptr::write_volatile(control, copy);
            sync::publish_u32(addr_of_mut!((*control).sequence), odd.wrapping_add(1));
        }
    }

    /// Reads a consistent snapshot, or `None` while a write is in progress
    /// or the magic/version do not match.
    ///
    /// # Safety
    ///
    /// `control` must point to a live, aligned, readable control mapping.
    pub unsafe fn snapshot(control: *const Self) -> Option<Self> {
        for _ in 0..4 {
            // SAFETY: Forwarded caller contract.
            let (before, record, after) = unsafe {
                let before = sync::observe_u32(addr_of!((*control).sequence));
                let record = core::ptr::read_volatile(control);
                sync::read_barrier();
                (before, record, sync::observe_u32(addr_of!((*control).sequence)))
            };
            if before == after && before & 1 == 0 {
                return (record.magic == CONTROL_MAGIC && record.version == CONTROL_VERSION).then_some(record);
            }
            core::hint::spin_loop();
        }
        None
    }
}

/// `DshowRequest::magic`: `"VCQ2"`.
pub const REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"VCQ2");
/// Current request layout version.
pub const REQUEST_VERSION: u32 = 1;
/// No consumer graph is connected.
pub const REQUEST_STATE_NONE: u32 = 0;
/// A consumer graph is connected with `format`.
pub const REQUEST_STATE_CONNECTED: u32 = 1;

/// Consumer-to-producer request, guarded by its own sequence lock. Written
/// only by the consumer-lease holder.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DshowRequest {
    /// [`REQUEST_MAGIC`].
    pub magic: u32,
    /// [`REQUEST_VERSION`].
    pub version: u32,
    /// `REQUEST_STATE_*`.
    pub state: u32,
    /// Sequence lock: odd while the consumer rewrites the record.
    pub sequence: u32,
    /// Format the consumer graph connected with.
    pub format: WireVideoFormat,
    /// Consumer process id (diagnostics only).
    pub consumer_pid: u32,
    /// Must be zero.
    pub reserved: [u32; 3],
}

const _: () = {
    assert!(size_of::<DshowRequest>() == 64);
    assert!(align_of::<DshowRequest>() == 64);
    assert!(offset_of!(DshowRequest, sequence) == 12);
    assert!(offset_of!(DshowRequest, format) == 16);
};

impl DshowRequest {
    /// Byte size of the request mapping.
    pub const SIZE: usize = size_of::<Self>();

    /// A record in `state` with `format`.
    pub fn new(state: u32, format: WireVideoFormat, consumer_pid: u32) -> Self {
        Self {
            magic: REQUEST_MAGIC,
            version: REQUEST_VERSION,
            state,
            sequence: 0,
            format,
            consumer_pid,
            reserved: [0; 3],
        }
    }

    /// Publishes `record` under the sequence lock.
    ///
    /// # Safety
    ///
    /// `request` must point to a live, aligned, writable request mapping
    /// with a single writer (the consumer-lease holder).
    pub unsafe fn publish(request: *mut Self, record: &Self) {
        // SAFETY: Forwarded caller contract.
        unsafe {
            let sequence = sync::observe_u32(addr_of!((*request).sequence));
            let odd = sequence.wrapping_add(1) | 1;
            sync::publish_u32(addr_of_mut!((*request).sequence), odd);
            let mut copy = *record;
            copy.sequence = odd;
            core::ptr::write_volatile(request, copy);
            sync::publish_u32(addr_of_mut!((*request).sequence), odd.wrapping_add(1));
        }
    }

    /// Reads a consistent snapshot, or `None` while a write is in progress
    /// or the magic/version do not match.
    ///
    /// # Safety
    ///
    /// `request` must point to a live, aligned, readable request mapping.
    pub unsafe fn snapshot(request: *const Self) -> Option<Self> {
        for _ in 0..4 {
            // SAFETY: Forwarded caller contract.
            let (before, record, after) = unsafe {
                let before = sync::observe_u32(addr_of!((*request).sequence));
                let record = core::ptr::read_volatile(request);
                sync::read_barrier();
                (before, record, sync::observe_u32(addr_of!((*request).sequence)))
            };
            if before == after && before & 1 == 0 {
                return (record.magic == REQUEST_MAGIC && record.version == REQUEST_VERSION).then_some(record);
            }
            core::hint::spin_loop();
        }
        None
    }
}

/// NUL-terminated UTF-16 names of one camera's DirectShow objects.
pub struct DshowNames {
    base: String,
    /// Producer lease event.
    pub lease: Vec<u16>,
    /// Fixed control mapping.
    pub control: Vec<u16>,
    /// Consumer lease event.
    pub consumer: Vec<u16>,
    /// Fixed request mapping.
    pub request: Vec<u16>,
}

impl DshowNames {
    /// Names for camera `id` in the session-local namespace.
    pub fn for_camera(id: CameraId) -> Self {
        Self::with_base(format!("Local\\VirtualCamRs2-{id}"))
    }

    /// Names under an arbitrary base (tests).
    pub fn with_base(base: String) -> Self {
        Self {
            lease: wide(&base, "-Lease"),
            control: wide(&base, "-Control"),
            consumer: wide(&base, "-Consumer"),
            request: wide(&base, "-Request"),
            base,
        }
    }

    /// Ring mapping name of session `generation`.
    pub fn ring(&self, generation: u64) -> Vec<u16> {
        wide(&self.base, &format!("-Ring-{generation:016X}"))
    }

    /// Frame event name of session `generation`.
    pub fn frame_event(&self, generation: u64) -> Vec<u16> {
        wide(&self.base, &format!("-Frame-{generation:016X}"))
    }
}

fn wide(base: &str, suffix: &str) -> Vec<u16> {
    base.encode_utf16().chain(suffix.encode_utf16()).chain(core::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{FrameRate, PixelFormat, VideoFormat};

    fn text(name: &[u16]) -> String {
        let (last, body) = name.split_last().expect("non-empty");
        assert_eq!(*last, 0);
        String::from_utf16(body).expect("utf16")
    }

    #[test]
    fn names_use_v2_session_local_namespace() {
        let id: CameraId = "{12345678-9ABC-DEF0-1122-334455667788}".parse().expect("id");
        let names = DshowNames::for_camera(id);
        let base = "Local\\VirtualCamRs2-{12345678-9ABC-DEF0-1122-334455667788}";
        assert_eq!(text(&names.lease), format!("{base}-Lease"));
        assert_eq!(text(&names.control), format!("{base}-Control"));
        assert_eq!(text(&names.ring(0xAB)), format!("{base}-Ring-00000000000000AB"));
        assert_eq!(text(&names.frame_event(0xAB)), format!("{base}-Frame-00000000000000AB"));
        assert_eq!(text(&names.consumer), format!("{base}-Consumer"));
        assert_eq!(text(&names.request), format!("{base}-Request"));
    }

    #[test]
    fn request_publish_and_snapshot_round_trip() {
        let mut request = DshowRequest::new(REQUEST_STATE_NONE, WireVideoFormat::default(), 0);
        request.magic = 0;
        // SAFETY: Live aligned local.
        assert_eq!(unsafe { DshowRequest::snapshot(&raw const request) }, None);
        let format = VideoFormat::new(PixelFormat::Yuy2, 640, 480, FrameRate::fps(30)).to_wire().expect("wire");
        let record = DshowRequest::new(REQUEST_STATE_CONNECTED, format, 42);
        // SAFETY: Live aligned local with a single writer.
        let snapshot = unsafe {
            DshowRequest::publish(&raw mut request, &record);
            DshowRequest::snapshot(&raw const request)
        }
        .expect("consistent");
        assert_eq!(snapshot.sequence, 2);
        assert_eq!(DshowRequest { sequence: 0, ..snapshot }, record);
    }

    #[test]
    fn control_publish_and_snapshot_round_trip() {
        let mut control = DshowControl {
            magic: 0,
            version: 0,
            state: 0,
            sequence: 0,
            generation: 0,
            producer_pid: 0,
            slot_count: 0,
            producer_start_time: 0,
            ring_size: 0,
            format: WireVideoFormat::default(),
            reserved: [0; 6],
        };
        // SAFETY: Live aligned local.
        assert_eq!(unsafe { DshowControl::snapshot(&raw const control) }, None);
        let record = DshowControl {
            magic: CONTROL_MAGIC,
            version: CONTROL_VERSION,
            state: CONTROL_STATE_ACTIVE,
            generation: 7,
            producer_pid: 9,
            slot_count: 3,
            ring_size: 4096,
            format: VideoFormat::new(PixelFormat::Yuy2, 640, 480, FrameRate::fps(30)).to_wire().expect("wire"),
            ..control
        };
        // SAFETY: Live aligned local with a single writer.
        let snapshot = unsafe {
            DshowControl::publish(&raw mut control, &record);
            DshowControl::snapshot(&raw const control)
        }
        .expect("consistent");
        assert_eq!(snapshot.sequence, 2);
        assert_eq!(DshowControl { sequence: 0, ..snapshot }, record);

        control.sequence = 3;
        // SAFETY: Live aligned local.
        assert_eq!(unsafe { DshowControl::snapshot(&raw const control) }, None, "odd sequence = writer active");
    }
}
