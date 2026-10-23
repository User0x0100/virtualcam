//! DirectShow control plane (transport v2).
//!
//! The producer owns the runtime format and every named object, all in the
//! session-local namespace:
//!
//! ```text
//! Local\VirtualCamRs2-{ID}-Lease               event; existence = single-writer lease
//! Local\VirtualCamRs2-{ID}-Control             fixed 128-byte DshowControl mapping
//! Local\VirtualCamRs2-{ID}-Ring-{GEN:016X}     per-session frame ring
//! ```
//!
//! Consumers never open the lease, so they can never make a producer's
//! occupancy check fail. Each producer session picks a fresh generation and
//! therefore a fresh ring name: a graph still holding an old ring never
//! constrains the next session's format or capacity.

use core::mem::{align_of, offset_of, size_of};
use core::ptr::{addr_of, addr_of_mut};

use crate::format::WireVideoFormat;
use crate::id::CameraId;
use crate::sync;

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

/// NUL-terminated UTF-16 names of one camera's DirectShow objects.
pub struct DshowNames {
    base: String,
    /// Producer lease event.
    pub lease: Vec<u16>,
    /// Fixed control mapping.
    pub control: Vec<u16>,
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
            base,
        }
    }

    /// Ring mapping name of session `generation`.
    pub fn ring(&self, generation: u64) -> Vec<u16> {
        wide(&self.base, &format!("-Ring-{generation:016X}"))
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
