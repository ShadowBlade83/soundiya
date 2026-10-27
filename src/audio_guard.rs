use std::{
    future::Future,
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::Notify;

/// Keeps the station's own music player quiet while this receiver is playing.
pub struct AudioGuard {
    playing: AtomicBool,
    changed: Notify,
}
impl AudioGuard {
    pub fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }
    pub fn set_playing(&self, playing: bool) {
        if self.playing.swap(playing, Ordering::AcqRel) != playing {
            self.changed.notify_one();
        }
    }
    pub async fn run(&self, address: std::net::SocketAddr) {
        self.run_with(|| crate::voice::pause_native(address)).await;
    }
    async fn run_with<F, Fut>(&self, mut pause: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failing = false;
        loop {
            tokio::select! { _=tick.tick()=>{}, _=self.changed.notified()=>{} }
            if !self.playing.load(Ordering::Acquire) {
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
        guard.set_playing(true);
        tokio::task::yield_now().await;
        for _ in 0..6 {
            tokio::time::advance(Duration::from_millis(500)).await;
            tokio::task::yield_now().await;
        }
        assert!(calls.load(Ordering::SeqCst) >= 6);
        guard.set_playing(false);
        tokio::task::yield_now().await;
        let paused = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), paused);
        worker.abort();
    }
}
