//! Delivery timing shared by the managed sources. Times are monotonic 100 ns units.

use crate::VideoFormat;

/// A frame's identity across producer reconnects.
pub type FrameId = (u64, u64);

/// Last processed frame and delivery time. Events are hints; sequence is authoritative.
#[derive(Default)]
pub struct Delivery {
    frame: Option<FrameId>,
    at: Option<u64>,
}

impl Delivery {
    /// Earliest next delivery: new frame (with 25% cadence jitter), idle black,
    /// or a one-second repeat when the producer stalls. Never catches up in bursts.
    pub fn due(&self, now: u64, latest: Option<FrameId>, interval: u64) -> u64 {
        let Some(last) = self.at else { return now };
        let delay = match latest.filter(|(_, sequence)| *sequence != 0) {
            Some(frame) if Some(frame) != self.frame => interval - interval / 4,
            Some(_) => 10_000_000,
            None => interval,
        };
        last.saturating_add(delay)
    }

    /// Records a delivered sample, including a stable frame replaced with black.
    pub fn delivered(&mut self, now: u64, frame: Option<FrameId>) {
        self.at = Some(now);
        self.frame = frame;
    }
}

/// A selected format only needs action when it differs from the producer's.
pub fn requested_format(producing: VideoFormat, selected: Option<VideoFormat>) -> Option<VideoFormat> {
    selected.filter(|selected| *selected != producing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameRate, PixelFormat};

    #[test]
    fn delivery_tracks_frames_reconnects_idle_and_stalls() {
        let mut delivery = Delivery::default();
        let interval = 400_000;
        assert_eq!(delivery.due(100, None, interval), 100);
        delivery.delivered(1_000_000, Some((1, 1)));
        assert_eq!(delivery.due(1_000_000, Some((1, 2)), interval), 1_300_000);
        assert_eq!(delivery.due(1_000_000, Some((2, 1)), interval), 1_300_000, "new generation restarts sequence");
        assert_eq!(delivery.due(1_000_000, Some((1, 1)), interval), 11_000_000, "duplicate event is not a new frame");
        assert_eq!(delivery.due(1_000_000, None, interval), 1_400_000);
        assert_eq!(delivery.due(1_000_000, Some((2, 0)), interval), 1_400_000, "no frame written yet");
        delivery.delivered(20_000_000, None);
        assert_eq!(delivery.due(20_000_000, None, interval), 20_400_000, "no catch-up burst");
    }

    #[test]
    fn format_requests_ignore_the_mode_already_written() {
        let native = VideoFormat::new(PixelFormat::Nv12, 64, 48, FrameRate::fps(30));
        let selected = VideoFormat { pixel_format: PixelFormat::Yuy2, ..native };
        assert_eq!(requested_format(native, None), None);
        assert_eq!(requested_format(native, Some(native)), None);
        assert_eq!(requested_format(native, Some(selected)), Some(selected));
    }
}
