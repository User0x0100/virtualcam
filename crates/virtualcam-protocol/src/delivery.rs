//! Delivery timing shared by the managed sources. Times are monotonic 100 ns units.
//!
//! New frames are rate-limited with a virtual schedule (GCRA): `next` is the
//! theoretical time of the next delivery and advances by exactly one interval
//! per delivery. A frame may go out up to a quarter interval before `next`, so
//! producer jitter adds no latency, yet the long-term rate never exceeds the
//! negotiated one (a 60 FPS producer into a 30 FPS camera yields 30 FPS, every
//! other frame). Early or slightly late deliveries keep the grid, so the
//! cadence does not drift; a delivery more than a quarter interval late
//! re-anchors it, so missed slots are never caught up in a burst.

use crate::VideoFormat;

/// A frame's identity across producer reconnects.
pub type FrameId = (u64, u64);

/// A stalled producer's last frame is repeated this often (100 ns units).
pub const REPEAT_100NS: u64 = 10_000_000;

/// Last processed frame and delivery schedule. Events are hints; sequence is authoritative.
#[derive(Default)]
pub struct Delivery {
    frame: Option<FrameId>,
    /// Time of the last delivery.
    at: Option<u64>,
    /// Theoretical time of the next delivery on the cadence grid.
    next: u64,
}

impl Delivery {
    /// Earliest next delivery: a new frame up to a quarter interval before the
    /// grid slot, black exactly on the grid, or a one-second repeat when the
    /// producer stalls.
    pub fn due(&self, now: u64, latest: Option<FrameId>, interval: u64) -> u64 {
        let Some(last) = self.at else { return now };
        match latest.filter(|(_, sequence)| *sequence != 0) {
            Some(frame) if Some(frame) != self.frame => self.next.saturating_sub(interval / 4),
            Some(_) => last.saturating_add(REPEAT_100NS),
            None => self.next,
        }
    }

    /// Records a delivered sample, including a stable frame replaced with black,
    /// and advances the grid by one interval.
    pub fn delivered(&mut self, now: u64, frame: Option<FrameId>, interval: u64) {
        // Up to a quarter interval late keeps the grid; later re-anchors at `now`.
        let anchor = if self.at.is_some() && now <= self.next.saturating_add(interval / 4) { self.next } else { now };
        self.next = anchor.saturating_add(interval);
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

    const INTERVAL: u64 = 400_000;

    /// Feeds frames published at `arrivals` and delivers each as soon as it is
    /// due (the source wakes on the frame event or the deadline). Returns the
    /// delivery times.
    fn simulate(arrivals: &[u64], end: u64) -> Vec<u64> {
        let mut delivery = Delivery::default();
        let mut delivered = Vec::new();
        let mut now = arrivals[0];
        while now < end {
            let published = arrivals.iter().filter(|arrival| **arrival <= now).count() as u64;
            let latest = Some((1, published));
            let due = delivery.due(now, latest, INTERVAL);
            if due <= now {
                delivery.delivered(now, latest, INTERVAL);
                delivered.push(now);
            }
            // Next wake: the next publication or the deadline, whichever is first.
            let next_arrival = arrivals.iter().copied().find(|arrival| *arrival > now).unwrap_or(u64::MAX);
            let deadline = delivery.due(now, latest, INTERVAL);
            now = next_arrival.min(if deadline > now { deadline } else { now + 1 });
        }
        delivered
    }

    #[test]
    fn faster_producer_is_capped_at_the_negotiated_rate() {
        // 60 FPS into a 30 FPS camera: exactly every other frame.
        let arrivals: Vec<u64> = (0..600).map(|n| n * INTERVAL / 2).collect();
        let delivered = simulate(&arrivals, 300 * INTERVAL);
        // One quarter-interval head start at the beginning, then the exact grid.
        assert_eq!(delivered.len(), 301);
        assert_eq!(delivered[1], INTERVAL - INTERVAL / 4);
        assert!(delivered[1..].windows(2).all(|pair| pair[1] - pair[0] == INTERVAL));
    }

    #[test]
    fn jittered_producer_is_delivered_without_delay_or_drops() {
        // 30 FPS with +-20% jitter around an exact grid.
        let jitter = [0, 80_000, 60_000, 20_000, 70_000, 10_000];
        let arrivals: Vec<u64> = (0..300).map(|n| 1_000_000 + n * INTERVAL + jitter[n as usize % 6] - 40_000).collect();
        let delivered = simulate(&arrivals, 1_000_000 + 300 * INTERVAL);
        assert_eq!(delivered, arrivals, "every frame goes out the moment it is published");
    }

    #[test]
    fn idle_black_follows_the_exact_grid_despite_late_wakeups() {
        let mut delivery = Delivery::default();
        let mut now = 1_000_000;
        for _ in 0..30 {
            assert!(delivery.due(now, None, INTERVAL) <= now);
            delivery.delivered(now, None, INTERVAL);
            // Wake 5 ms (an eighth interval) after each deadline.
            now = delivery.due(now, None, INTERVAL) + INTERVAL / 8;
        }
        assert_eq!(delivery.due(now, None, INTERVAL), 1_000_000 + 30 * INTERVAL, "late wakeups do not drift the grid");
    }

    #[test]
    fn delivery_tracks_frames_reconnects_idle_and_stalls() {
        let mut delivery = Delivery::default();
        assert_eq!(delivery.due(100, None, INTERVAL), 100);
        delivery.delivered(1_000_000, Some((1, 1)), INTERVAL);
        assert_eq!(delivery.due(1_000_000, Some((1, 2)), INTERVAL), 1_300_000);
        assert_eq!(delivery.due(1_000_000, Some((2, 1)), INTERVAL), 1_300_000, "new generation restarts sequence");
        assert_eq!(delivery.due(1_000_000, Some((1, 1)), INTERVAL), 11_000_000, "duplicate event is not a new frame");
        assert_eq!(delivery.due(1_000_000, None, INTERVAL), 1_400_000);
        assert_eq!(delivery.due(1_000_000, Some((2, 0)), INTERVAL), 1_400_000, "no frame written yet");
        delivery.delivered(20_000_000, None, INTERVAL);
        assert_eq!(delivery.due(20_000_000, None, INTERVAL), 20_400_000, "no catch-up burst after a long gap");
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
