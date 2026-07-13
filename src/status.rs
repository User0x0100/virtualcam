/// Non-fatal outcome of an attempted frame publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    /// The frame was copied and published through the backend protocol.
    Written,
    /// Unity Capture's receiver-side objects do not exist yet.
    ConsumerInactive,
    /// Unity Capture accepted the frame, but no pending want-frame event was observed.
    FrameSkippedByConsumer,
}
