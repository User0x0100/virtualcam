use core::mem::{align_of, offset_of, size_of};

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
pub(crate) struct ManagedControlHeader {
    pub(crate) magic: u32,
    pub(crate) version: u32,
    pub(crate) state: u32,
    pub(crate) reserved: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) interval_100ns: u64,
}

impl ManagedControlHeader {
    pub(crate) const fn new(width: u32, height: u32, interval_100ns: u64) -> Self {
        Self {
            magic: CONTROL_MAGIC,
            version: CONTROL_VERSION,
            state: CONTROL_STATE_STARTING,
            reserved: 0,
            width,
            height,
            interval_100ns,
        }
    }
}

const _: () = {
    assert!(size_of::<ManagedControlHeader>() == 32);
    assert!(align_of::<ManagedControlHeader>() == 8);
    assert!(offset_of!(ManagedControlHeader, magic) == 0);
    assert!(offset_of!(ManagedControlHeader, version) == 4);
    assert!(offset_of!(ManagedControlHeader, state) == 8);
    assert!(offset_of!(ManagedControlHeader, width) == 16);
    assert!(offset_of!(ManagedControlHeader, height) == 20);
    assert!(offset_of!(ManagedControlHeader, interval_100ns) == 24);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_header_has_fixed_cross_language_layout() {
        let header = ManagedControlHeader::new(1920, 1080, 333_333);
        assert_eq!(size_of::<ManagedControlHeader>(), 32);
        assert_eq!(align_of::<ManagedControlHeader>(), 8);
        assert_eq!(header.magic, CONTROL_MAGIC);
        assert_eq!(header.version, CONTROL_VERSION);
        assert_eq!(header.state, CONTROL_STATE_STARTING);
        assert_eq!((header.width, header.height), (1920, 1080));
        assert_eq!(header.interval_100ns, 333_333);
    }
}
