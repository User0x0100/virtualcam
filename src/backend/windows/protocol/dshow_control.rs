//! Producer-owned control plane of the managed DirectShow camera.
//!
//! The producer chooses the runtime format, so it owns every named object in
//! the session-local `Local\VirtualCamRs-{CLSID}` namespace. This control plane
//! is DirectShow-specific; a Media Foundation source owns its own control and
//! must not reuse these names or this header.

use core::mem::{align_of, offset_of, size_of};

use crate::ManagedCameraId;
use crate::config::Nv12Format;

pub(crate) const CONTROL_MAGIC: u32 = u32::from_le_bytes(*b"VCMR");
pub(crate) const CONTROL_VERSION: u32 = 1;
pub(crate) const CONTROL_STATE_STARTING: u32 = 1;
pub(crate) const CONTROL_STATE_READY: u32 = 2;
pub(crate) const CONTROL_STATE_STOPPING: u32 = 3;

/// Stable, fixed-size control plane for one managed camera producer session.
///
/// The large OBS-compatible frame queue is format-specific. Keeping only this
/// small declaration at the stable CLSID-derived name lets old DirectShow graphs
/// retain their prior data mapping without constraining the next producer's
/// resolution or frame rate.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DshowControlHeader {
    pub(crate) magic: u32,
    pub(crate) version: u32,
    pub(crate) state: u32,
    pub(crate) reserved: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) interval_100ns: u64,
}

impl DshowControlHeader {
    pub(crate) const fn starting(format: Nv12Format) -> Self {
        Self {
            magic: CONTROL_MAGIC,
            version: CONTROL_VERSION,
            state: CONTROL_STATE_STARTING,
            reserved: 0,
            width: format.width,
            height: format.height,
            interval_100ns: format.interval_100ns,
        }
    }
}

const _: () = {
    assert!(size_of::<DshowControlHeader>() == 32);
    assert!(align_of::<DshowControlHeader>() == 8);
    assert!(offset_of!(DshowControlHeader, magic) == 0);
    assert!(offset_of!(DshowControlHeader, version) == 4);
    assert!(offset_of!(DshowControlHeader, state) == 8);
    assert!(offset_of!(DshowControlHeader, width) == 16);
    assert!(offset_of!(DshowControlHeader, height) == 20);
    assert!(offset_of!(DshowControlHeader, interval_100ns) == 24);
};

/// NUL-terminated UTF-16 names of one camera's DirectShow transport objects.
///
/// ```text
/// Local\VirtualCamRs-{CLSID}-Producer
/// Local\VirtualCamRs-{CLSID}-Ready
/// Local\VirtualCamRs-{CLSID}-Control
/// Local\VirtualCamRs-{CLSID}-Data-{width}x{height}-{interval}
/// ```
pub(crate) struct DshowObjectNames {
    base: String,
    pub(crate) producer: Vec<u16>,
    pub(crate) ready: Vec<u16>,
    pub(crate) control: Vec<u16>,
}

impl DshowObjectNames {
    pub(crate) fn for_camera(id: ManagedCameraId) -> Self {
        Self::with_base(format!("Local\\VirtualCamRs-{id}"))
    }

    pub(crate) fn with_base(base: String) -> Self {
        Self {
            producer: wide(&base, "-Producer"),
            ready: wide(&base, "-Ready"),
            control: wide(&base, "-Control"),
            base,
        }
    }

    /// Format-specific data mapping, so retained old sections never constrain a
    /// later producer's capacity.
    pub(crate) fn data(&self, format: Nv12Format) -> Vec<u16> {
        wide(&self.base, &format!("-Data-{}x{}-{}", format.width, format.height, format.interval_100ns))
    }
}

fn wide(base: &str, suffix: &str) -> Vec<u16> {
    base.encode_utf16().chain(suffix.encode_utf16()).chain(core::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FrameRate;

    fn utf8(name: &[u16]) -> String {
        let (last, text) = name.split_last().expect("non-empty name");
        assert_eq!(*last, 0, "object names are NUL-terminated");
        String::from_utf16(text).expect("UTF-16 object name")
    }

    #[test]
    fn control_header_has_fixed_cross_language_layout() {
        let format = Nv12Format::new(1920, 1080, FrameRate { numerator: 30, denominator: 1 }).expect("valid format");
        let header = DshowControlHeader::starting(format);
        assert_eq!(size_of::<DshowControlHeader>(), 32);
        assert_eq!(align_of::<DshowControlHeader>(), 8);
        assert_eq!(header.magic, CONTROL_MAGIC);
        assert_eq!(header.version, CONTROL_VERSION);
        assert_eq!(header.state, CONTROL_STATE_STARTING);
        assert_eq!((header.width, header.height), (1920, 1080));
        assert_eq!(header.interval_100ns, 333_333);
    }

    #[test]
    fn object_names_match_native_namespace() {
        let id: ManagedCameraId = "{12345678-9ABC-DEF0-1122-334455667788}".parse().expect("valid CLSID");
        let names = DshowObjectNames::for_camera(id);
        let base = "Local\\VirtualCamRs-{12345678-9ABC-DEF0-1122-334455667788}";
        assert_eq!(utf8(&names.producer), format!("{base}-Producer"));
        assert_eq!(utf8(&names.ready), format!("{base}-Ready"));
        assert_eq!(utf8(&names.control), format!("{base}-Control"));
        let format = Nv12Format::new(1280, 720, FrameRate { numerator: 60, denominator: 1 }).expect("valid format");
        assert_eq!(utf8(&names.data(format)), format!("{base}-Data-1280x720-166666"));
    }
}
