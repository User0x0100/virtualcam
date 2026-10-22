//! Wire-level shared-memory protocols.
//!
//! These modules hold only layouts, names, and publication order; they own no
//! handles, so they can later move into a standalone `managed-protocol` crate
//! shared with the Rust DirectShow and Media Foundation sources.

pub(crate) mod dshow_control;
pub(crate) mod frame_queue;
pub(crate) mod publication;
pub(crate) mod unity_capture;
