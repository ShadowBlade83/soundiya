use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::time::Instant;

const SETTLE_MS: u32 = 750;

pub struct VolumeGuard {
    enabled: AtomicBool,
    busy: AtomicBool,
    holding: AtomicBool,
    native_command: AtomicBool,
    release_at: AtomicU32,
    origin: Instant,
}
impl VolumeGuard {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            holding: AtomicBool::new(false),
            native_command: AtomicBool::new(false),
            release_at: AtomicU32::new(0),
            origin: Instant::now(),
        }
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if !enabled {
            self.holding.store(false, Ordering::Release);
        } else if self.busy.load(Ordering::Acquire) {
            self.holding.store(true, Ordering::Release);
        }
    }
    pub fn activity(&self, busy: bool) {
        let previous = self.busy.swap(busy, Ordering::AcqRel);
        if busy {
            if !previous {
                self.native_command.store(false, Ordering::Release);
            }
            if self.enabled.load(Ordering::Acquire) {
                self.holding.store(true, Ordering::Release);
            }
            self.release_at.store(0, Ordering::Release);
        } else if previous && self.holding() {
            self.release_at.store(
                (self.origin.elapsed().as_millis() as u32)
                    .wrapping_add(SETTLE_MS)
                    .max(1),
                Ordering::Release,
            );
        }
    }
    pub fn holding(&self) -> bool {
        self.holding.load(Ordering::Acquire)
    }
    pub fn native_command(&self) {
        self.native_command.store(true, Ordering::Release);
    }
    pub fn app_command(&self) {
        self.native_command.store(false, Ordering::Release);
    }
    pub fn restore_target(&self, actual: u16, desired: u16) -> Option<u16> {
        let at = self.release_at.load(Ordering::Acquire);
        if !self.holding()
            || self.busy.load(Ordering::Acquire)
            || at == 0
            || ((self.origin.elapsed().as_millis() as u32).wrapping_sub(at) as i32) < 0
        {
            return None;
        }
        Some(if self.native_command.load(Ordering::Acquire) {
            actual
        } else {
            desired
        })
    }
    pub fn restored(&self) {
        self.holding.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[tokio::test(start_paused = true)]
    async fn weather_answer_does_not_replace_player_volume_with_saved_stock_level() {
        let guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true);
        assert!(guard.holding());
        // Master is 40% for speech, then the native player restores its stale 10%.
        assert_eq!(guard.restore_target(40, 37), None);
        assert_eq!(guard.restore_target(10, 37), None);
        guard.activity(false);
        assert_eq!(guard.restore_target(10, 37), None);
        tokio::time::advance(Duration::from_millis(SETTLE_MS.into())).await;
        assert_eq!(guard.restore_target(10, 37), Some(37));
        guard.restored();
        assert!(!guard.holding());
    }
    #[tokio::test(start_paused = true)]
    async fn voice_volume_is_kept_but_later_player_changes_take_precedence() {
        let guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true);
        guard.native_command();
        guard.activity(false);
        tokio::time::advance(Duration::from_millis(SETTLE_MS.into())).await;
        assert_eq!(guard.restore_target(30, 37), Some(30));
        guard.app_command();
        assert_eq!(guard.restore_target(30, 52), Some(52));
        guard.set_enabled(false);
        assert!(!guard.holding());
        assert_eq!(guard.restore_target(30, 52), None);
    }
    #[tokio::test(start_paused = true)]
    async fn followup_question_cancels_pending_restore() {
        let guard = VolumeGuard::new();
        guard.set_enabled(true);
        guard.activity(true);
        guard.activity(false);
        tokio::time::advance(Duration::from_millis(500)).await;
        guard.activity(true);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(guard.restore_target(10, 37), None);
        guard.activity(false);
        tokio::time::advance(Duration::from_millis(SETTLE_MS.into())).await;
        assert_eq!(guard.restore_target(10, 37), Some(37));
    }
}
