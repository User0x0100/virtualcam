//! Frame publication shared by both managed producers.

use virtualcam_protocol::ring::RingWriter;
use windows_sys::Win32::System::Threading::SetEvent;

use super::FrameSource;
use crate::platform::windows::OwnedHandle;
use crate::{Error, Result, WriteStatus};

pub(super) fn write_frame(writer: &mut RingWriter, event: &OwnedHandle, frame: FrameSource<'_>, timestamp: u64) -> Result<WriteStatus> {
    if frame.byte_len() != writer.frame_size() {
        return Err(Error::FrameSizeMismatch {
            expected: writer.frame_size(),
            actual: frame.byte_len(),
        });
    }
    let payload = writer.begin_frame();
    if let Err(error) = frame.copy_to(payload) {
        writer.abort_frame();
        return Err(error);
    }
    writer.commit_frame(timestamp);
    // SAFETY: Session-owned event; commit publishes the frame before notification.
    if unsafe { SetEvent(event.raw()) } == 0 {
        return Err(crate::platform::windows::last_error("SetEvent(managed frame)"));
    }
    Ok(WriteStatus::Written)
}
