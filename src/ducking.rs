use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};
use tokio::time::Instant;

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
const HOLD: Duration = Duration::from_secs(60);
const LEVEL: f64 = 0.2;
const ATTACK_FRAMES: f64 = SAMPLE_RATE as f64 * 0.08;
const RELEASE_FRAMES: f64 = SAMPLE_RATE as f64 * 0.3;

pub struct Ducking {
    origin: Instant,
    until: AtomicU32,
    pub volume: crate::volume_guard::VolumeGuard,
}
impl Ducking {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            until: AtomicU32::new(0),
            volume: crate::volume_guard::VolumeGuard::new(),
        }
    }
    pub fn set_busy(&self, busy: bool) {
        let until = if busy {
            (self.origin.elapsed().as_millis() as u32)
                .wrapping_add(HOLD.as_millis() as u32)
                .max(1)
        } else {
            0
        };
        self.until.store(until, Ordering::Release);
        self.volume.activity(busy);
    }
    pub fn active(&self) -> bool {
        let until = self.until.load(Ordering::Acquire);
        until != 0 && until.wrapping_sub(self.origin.elapsed().as_millis() as u32) as i32 > 0
    }
}

pub struct Gain(f64);
impl Gain {
    pub fn new() -> Self {
        Self(1.0)
    }
    fn step(&mut self, target: f64) {
        let step = (1.0 - LEVEL)
            / if target < self.0 {
                ATTACK_FRAMES
            } else {
                RELEASE_FRAMES
            };
        self.0 = if target < self.0 {
            (self.0 - step).max(target)
        } else {
            (self.0 + step).min(target)
        };
    }
    /// Ramps the gain towards the ducked level while Alice is active, one step per frame.
    pub fn apply_i16(&mut self, samples: &mut [i16], busy: bool) {
        let target = if busy { LEVEL } else { 1.0 };
        if self.0 == 1.0 && target == 1.0 {
            return;
        }
        for frame in samples.chunks_exact_mut(CHANNELS) {
            self.step(target);
            for sample in frame {
                *sample = (f64::from(*sample) * self.0) as i16;
            }
        }
    }
    #[cfg(test)]
    fn apply(&mut self, samples: &mut [f64], busy: bool) {
        let target = if busy { LEVEL } else { 1.0 };
        if self.0 == 1.0 && target == 1.0 {
            return;
        }
        for frame in samples.chunks_exact_mut(CHANNELS) {
            self.step(target);
            for sample in frame {
                *sample *= self.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn idle_disconnect_and_missing_idle_event_restore_volume() {
        let duck = Ducking::new();
        assert!(!duck.active());
        duck.set_busy(true);
        assert!(duck.active());
        tokio::time::advance(HOLD - Duration::from_secs(1)).await;
        assert!(duck.active());
        duck.set_busy(false);
        assert!(!duck.active());
        duck.set_busy(true);
        tokio::time::advance(HOLD).await;
        assert!(!duck.active());
    }
    #[test]
    fn ramps_are_stereo_linked_and_bounded() {
        let mut gain = Gain::new();
        let mut whole = vec![1.0; 10000];
        gain.apply(&mut whole, true);
        assert!(whole[0] < 1.0 && whole[0] > 0.99);
        assert_eq!(whole[9999], LEVEL);
        assert!(whole.windows(2).all(|w| w[0] >= w[1]));
        let mut pcm = vec![10_000i16; 40_000];
        Gain::new().apply_i16(&mut pcm, true);
        assert_eq!(pcm[39_999], 2_000);
    }
}
