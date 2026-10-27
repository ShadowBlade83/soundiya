use crate::{config::Audio, ducking::Ducking};
use alsa::mixer::SelemId;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU16, Ordering},
};

#[cfg(test)]
pub fn percent(volume: u16) -> u16 {
    ((u32::from(volume) * 100 + 32767) / 65535) as u16
}
pub fn from_percent(value: u16) -> u16 {
    ((u32::from(value.min(100)) * 65535 + 50) / 100) as u16
}

/// The station's master volume, shared with Alice and the physical buttons.
pub struct DeviceMixer {
    audio: Audio,
    handle: Mutex<Option<alsa::mixer::Mixer>>,
    /// The level the listener chose, remembered while Alice temporarily lowers the real one.
    pub desired: AtomicU16,
    ducking: Arc<Ducking>,
}
impl DeviceMixer {
    pub fn new(audio: Audio, ducking: Arc<Ducking>) -> Self {
        Self {
            audio,
            handle: Mutex::new(None),
            desired: AtomicU16::new(0),
            ducking,
        }
    }
    fn with_control<T>(
        &self,
        action: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, alsa::Error>,
    ) -> Result<T, alsa::Error> {
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| alsa::Error::new("ALSA mixer lock", libc::EIO))?;
        if handle.is_none() {
            *handle = Some(alsa::mixer::Mixer::new(&self.audio.mixer_device, true)?);
        }
        let result = (|| {
            let mixer = handle.as_ref().unwrap();
            mixer.handle_events()?;
            let control = mixer
                .find_selem(&SelemId::new(
                    &self.audio.mixer_control,
                    self.audio.mixer_index,
                ))
                .ok_or_else(|| alsa::Error::new("ALSA mixer control", libc::ENOENT))?;
            action(&control)
        })();
        if result.is_err() {
            *handle = None;
        }
        result
    }
    pub fn read(&self) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            let value = control.get_playback_volume(self.audio.channel())?;
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            Ok(((value - min).clamp(0, max - min) * 65535 / (max - min)) as u16)
        })
    }
    pub fn write(&self, volume: u16) -> Result<u16, alsa::Error> {
        self.with_control(|control| {
            let (min, max) = control.get_playback_volume_range();
            if max <= min {
                return Err(alsa::Error::new("ALSA mixer range", libc::EINVAL));
            }
            let value = min + (i64::from(volume) * (max - min) + 32767) / 65535;
            control.set_playback_volume_all(value)?;
            Ok(((value - min) * 65535 / (max - min)) as u16)
        })
    }
    /// Sets the volume from our own CLI; while Alice holds the level only the target moves.
    pub fn set(&self, value: u16) {
        self.ducking.volume.app_command();
        if self.ducking.volume.holding() {
            self.desired.store(value, Ordering::Release);
        } else if let Ok(actual) = self.write(value) {
            self.desired.store(actual, Ordering::Release);
        } else {
            eprintln!("mixer: volume write failed");
        }
    }
    /// Called every tick: remembers the listener's level and restores it after Alice spoke.
    pub fn follow(&self, playing: bool) {
        let guard = &self.ducking.volume;
        guard.set_enabled(playing);
        guard.activity(self.ducking.active());
        let Ok(value) = self.read() else {
            return;
        };
        if guard.holding() {
            let desired = self.desired.load(Ordering::Acquire);
            if let Some(target) = guard.restore_target(value, desired)
                && let Ok(actual) = if target == value { Ok(value) } else { self.write(target) }
            {
                self.desired.store(actual, Ordering::Release);
                guard.restored();
                eprintln!("volume: restored after Alice conversation");
            }
        } else {
            self.desired.store(value, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percent_conversion_round_trips() {
        for p in 0..=100 {
            assert_eq!(percent(from_percent(p)), p);
        }
        assert_eq!(from_percent(250), 65535);
    }
}
