//! Wire-level shared-memory protocols.
//!
//! Third-party protocols (OBS queue, Unity Capture). These modules hold only
//! layouts, names, and publication order and own no handles. This crate's own
//! managed-camera transport lives in the `virtualcam-protocol` crate.

pub(crate) mod frame_queue;
pub(crate) mod publication;
pub(crate) mod unity_capture;
