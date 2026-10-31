use std::{
    future::Future,
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::Notify;

/// How often the station's player is paused again while nothing prompts it. Every pause is a
/// new connection to Alice's platform daemon, so a tight loop here loads her and makes her slow.
const PERIOD: Duration = Duration::from_secs(10);

/// Keeps the station's own music player quiet while this receiver is playing.
pub struct AudioGuard {
    playing: AtomicBool,
    changed: Notify,
    /// No pauses before this moment: a pause command would cut Alice off mid-sentence.
    hold_until: std::sync::Mutex<Option<tokio::time::Instant>>,
}
impl AudioGuard {
    pub fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            changed: Notify::new(),
            hold_until: std::sync::Mutex::new(None),
        }
    }
    /// Leaves the station's player alone for a while, for when Alice is about to speak.
    pub fn hold(&self, duration: Duration) {
        *self.hold_until.lock().unwrap() = Some(tokio::time::Instant::now() + duration);
    }
    /// Ends a `hold` early, once Alice has said what she had to: a station player that
    /// started meanwhile (her own answer to another phrase, say) is paused at once.
    pub fn release(&self) {
        *self.hold_until.lock().unwrap() = None;
        self.changed.notify_one();
    }
    fn held(&self) -> bool {
        self.hold_until.lock().unwrap().is_some_and(|until| tokio::time::Instant::now() < until)
    }
    pub fn set_playing(&self, playing: bool) {
        if self.playing.swap(playing, Ordering::AcqRel) != playing {
            self.changed.notify_one();
        }
    }
    /// Asks for an extra pause right away; used when Alice has just finished answering, the
    /// moment her own player may have started.
    pub fn nudge(&self) {
        self.changed.notify_one();
    }
    pub async fn run(&self, address: std::net::SocketAddr) {
        self.run_with(|| crate::voice::pause_native(address)).await;
    }
    async fn run_with<F, Fut>(&self, mut pause: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let mut tick = tokio::time::interval(PERIOD);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failing = false;
        loop {
            tokio::select! { _=tick.tick()=>{}, _=self.changed.notified()=>{} }
            if !self.playing.load(Ordering::Acquire) || self.held() {
                continue;
            }
            match pause().await {
                Ok(()) => failing = false,
                Err(_) if !failing => {
                    failing = true;
                    eprintln!("audio: native music pause failed; retrying");
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicUsize};
    #[tokio::test(start_paused = true)]
    async fn pauses_native_music_only_while_playing() {
        let guard = Arc::new(AudioGuard::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = {
            let g = guard.clone();
            let c = calls.clone();
            tokio::spawn(async move {
                g.run_with(|| {
                    c.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                })
                .await;
            })
        };
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Starting to play pauses at once, then only once per period.
        guard.set_playing(true);
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for _ in 0..30 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        let steady = calls.load(Ordering::SeqCst);
        assert!((3..=5).contains(&steady), "{steady} pauses in 30 s");
        guard.nudge();
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), steady + 1);
        // While Alice speaks for us nothing is paused; the end of her sentence pauses at once.
        guard.hold(Duration::from_secs(20));
        guard.nudge();
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), steady + 1);
        guard.release();
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), steady + 2);
        guard.set_playing(false);
        tokio::task::yield_now().await;
        let paused = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), paused);
        worker.abort();
    }
}
