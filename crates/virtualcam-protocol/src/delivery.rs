//! Delivery timing shared by the managed sources. Times are monotonic 100 ns units.
//!
//! New frames are rate-limited with a virtual schedule (GCRA): `next` is the
//! theoretical time of the next delivery and advances by exactly one interval
//! per delivery. A new frame is judged when it is first seen: up to a quarter
//! interval before `next` (or any time later) it goes out at once, so producer
//! jitter adds no latency; earlier than that it is skipped, never delayed, and
//! the next frame is judged instead. The long-term rate therefore never exceeds
//! the negotiated one (a 60 FPS producer into a 30 FPS camera yields 30 FPS,
//! every other frame), and a grid whose phase differs from the producer's (set
//! by idle black, say) locks onto the producer after at most one skipped frame
//! instead of holding every frame back. Deliveries up to a quarter interval
//! late keep the grid, so the cadence does not drift on late wakeups; later
//! ones re-anchor it at the delivery, so missed slots are never caught up in a
//! burst.

use crate::VideoFormat;

/// A frame's identity across producer reconnects.
pub type FrameId = (u64, u64);

/// A stalled producer's last frame is repeated this often (100 ns units).
pub const REPEAT_100NS: u64 = 10_000_000;

/// Last processed frame and delivery schedule. Events are hints; sequence is authoritative.
#[derive(Default)]
pub struct Delivery {
    frame: Option<FrameId>,
    /// A newer frame that arrived too early and is not delivered.
    skipped: Option<FrameId>,
    /// Time of the last delivery.
    at: Option<u64>,
    /// Theoretical time of the next delivery on the cadence grid.
    next: u64,
}

impl Delivery {
    /// Earliest next delivery: a conforming new frame at once, black exactly on
    /// the grid, or a one-second repeat while the producer stalls (or its newest
    /// frame was skipped). Call it when a frame may have arrived: the first call
    /// that sees a frame decides whether it conforms.
    pub fn due(&mut self, now: u64, latest: Option<FrameId>, interval: u64) -> u64 {
        let Some(last) = self.at else { return now };
        match latest.filter(|(_, sequence)| *sequence != 0) {
            Some(frame) if Some(frame) != self.frame && Some(frame) != self.skipped => {
                if now >= self.next.saturating_sub(interval / 4) {
                    return now;
                }
                self.skipped = Some(frame);
                last.saturating_add(REPEAT_100NS)
            }
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
        self.skipped = None;
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

    /// One delivered sample: when, and the publication time of the producer
    /// frame it carried (`None` for black).
    #[derive(Debug, PartialEq)]
    struct Sample {
        at: u64,
        published: Option<u64>,
    }

    /// Runs a source from time 0 (black until the first publication) that
    /// wakes on every publication and on its deadline, delivering whenever due.
    fn simulate(arrivals: &[u64], end: u64) -> Vec<Sample> {
        let mut delivery = Delivery::default();
        let mut samples = Vec::new();
        let mut now = 0;
        while now < end {
            let published = arrivals.iter().filter(|arrival| **arrival <= now).count();
            let latest = (published > 0).then_some((1, published as u64));
            if delivery.due(now, latest, INTERVAL) <= now {
                delivery.delivered(now, latest, INTERVAL);
                samples.push(Sample {
                    at: now,
                    published: published.checked_sub(1).map(|index| arrivals[index]),
                });
            }
            let next_arrival = arrivals.iter().copied().find(|arrival| *arrival > now).unwrap_or(u64::MAX);
            let deadline = delivery.due(now, latest, INTERVAL);
            now = next_arrival.min(deadline.max(now + 1));
        }
        samples
    }

    fn frames(samples: &[Sample]) -> Vec<&Sample> {
        samples.iter().filter(|sample| sample.published.is_some()).collect()
    }

    #[test]
    fn faster_producer_is_capped_at_the_negotiated_rate() {
        // 60 FPS into a 30 FPS camera, in phase: exactly every other frame, at once.
        let arrivals: Vec<u64> = (0..600).map(|n| n * INTERVAL / 2).collect();
        let samples = simulate(&arrivals, 300 * INTERVAL);
        let frames = frames(&samples);
        assert_eq!(frames.len(), 300);
        assert!(frames.iter().all(|sample| sample.published == Some(sample.at)), "no added latency");
        assert!(frames.windows(2).all(|pair| pair[1].at - pair[0].at == INTERVAL));
    }

    #[test]
    fn producer_out_of_phase_with_idle_black_is_not_held_back() {
        // Black sets the grid at 0, T, 2T, ...; the producer then publishes at
        // 30 FPS, 0.7 interval ahead of the grid. Holding early frames until the
        // grid would add 0.45 interval to every frame forever.
        let arrivals: Vec<u64> = (0..100).map(|n| 3 * INTERVAL + 3 * INTERVAL / 10 + n * INTERVAL).collect();
        let samples = simulate(&arrivals, 104 * INTERVAL);
        let frames = frames(&samples);
        assert!(frames.len() >= 99, "at most the first frame is skipped while the grid locks on");
        assert!(frames.iter().all(|sample| sample.published == Some(sample.at)), "every delivered frame goes out at once");
        assert!(samples.iter().skip_while(|sample| sample.published.is_none()).all(|sample| sample.published.is_some()), "no black once the producer runs");
    }

    #[test]
    fn jittered_producer_is_delivered_without_delay_or_drops() {
        // 30 FPS with +-10% jitter around an exact grid, in phase with idle black.
        let jitter = [0, 80_000, 60_000, 20_000, 70_000, 10_000];
        let arrivals: Vec<u64> = (0..300).map(|n| 1_200_000 + n * INTERVAL + jitter[n as usize % 6] - 40_000).collect();
        let samples = simulate(&arrivals, 1_200_000 + 300 * INTERVAL);
        let delivered: Vec<u64> = frames(&samples).iter().map(|sample| sample.at).collect();
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
        assert_eq!(delivery.due(1_000_000, Some((1, 1)), INTERVAL), 11_000_000, "duplicate event is not a new frame");
        assert_eq!(delivery.due(1_000_000, None, INTERVAL), 1_400_000);
        assert_eq!(delivery.due(1_000_000, Some((2, 0)), INTERVAL), 1_400_000, "no frame written yet");
        assert_eq!(delivery.due(1_300_000, Some((2, 1)), INTERVAL), 1_300_000, "new generation restarts sequence");
        assert_eq!(delivery.due(1_100_000, Some((1, 2)), INTERVAL), 11_000_000, "too early: skipped, not delayed");
        assert_eq!(delivery.due(1_350_000, Some((1, 2)), INTERVAL), 11_000_000, "a skipped frame stays skipped");
        assert_eq!(delivery.due(1_350_000, Some((1, 3)), INTERVAL), 1_350_000, "the next frame is judged afresh");
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
