//! The station's touch buttons. While soundiya has music loaded it grabs the input device, so
//! Alice no longer hears the top button (and no longer answers that Plus is missing). A tap on
//! it pauses and resumes; the volume buttons keep working, and Alice-button plus a volume
//! button skips tracks. With nothing loaded the buttons are left to Alice.

use crate::{
    mixer::DeviceMixer,
    voice::{Event, Intent},
};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Read,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const ALICE: u16 = 582;
const VOLUME_UP: u16 = 115;
const VOLUME_DOWN: u16 = 114;
/// One volume press moves the level by about 5 %.
const STEP: u16 = 3277;
const REPEAT_DELAY: Duration = Duration::from_millis(450);
const REPEAT: Duration = Duration::from_millis(180);

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_DROPPED: u16 = 3;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
/// EVIOCGBIT(EV_KEY, 96): which keys the device can report.
const EVIOCGBIT_KEY: libc::c_ulong = 0x8060_4521;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Toggle,
    Next,
    Previous,
    Louder,
    Quieter,
}

/// Turns key events into actions: a tap on the Alice button toggles playback, a volume button
/// repeats while held, and a volume button pressed during an Alice hold skips instead.
#[derive(Default)]
struct Gesture {
    alice_down: bool,
    alice_used: bool,
    repeating: Option<(u16, Instant)>,
}
impl Gesture {
    fn key(&mut self, code: u16, value: i32, now: Instant) -> Option<Action> {
        match (code, value) {
            (ALICE, 1) => {
                self.alice_down = true;
                self.alice_used = false;
                None
            }
            (ALICE, 0) => {
                let tap = self.alice_down && !self.alice_used;
                self.alice_down = false;
                tap.then_some(Action::Toggle)
            }
            (VOLUME_UP | VOLUME_DOWN, 1) => {
                let up = code == VOLUME_UP;
                if self.alice_down {
                    self.alice_used = true;
                    self.repeating = None;
                    Some(if up { Action::Next } else { Action::Previous })
                } else {
                    self.repeating = Some((code, now + REPEAT_DELAY));
                    Some(if up { Action::Louder } else { Action::Quieter })
                }
            }
            (VOLUME_UP | VOLUME_DOWN, 0) => {
                if matches!(self.repeating, Some((held, _)) if held == code) {
                    self.repeating = None;
                }
                None
            }
            _ => None,
        }
    }
    fn tick(&mut self, now: Instant) -> Option<Action> {
        let (code, at) = self.repeating?;
        if now < at {
            return None;
        }
        self.repeating = Some((code, now + REPEAT));
        Some(if code == VOLUME_UP { Action::Louder } else { Action::Quieter })
    }
}

/// The event device that has the Alice key, whatever this model calls its touch sensor.
fn find_device() -> Option<(String, File)> {
    (0..32).find_map(|n| {
        let path = format!("/dev/input/event{n}");
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
            .ok()?;
        let mut keys = [0u8; 96];
        let result = unsafe { libc::ioctl(file.as_raw_fd(), EVIOCGBIT_KEY as _, keys.as_mut_ptr()) };
        let has_alice = result >= 0 && keys[usize::from(ALICE) / 8] & (1 << (ALICE % 8)) != 0;
        has_alice.then_some((path, file))
    })
}

/// Starts the button thread. `active` says whether music is loaded and the buttons are ours.
pub fn spawn(sender: mpsc::Sender<Event>, mixer: Arc<DeviceMixer>, active: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("buttons".into())
        .spawn(move || run(sender, mixer, active));
    if spawned.is_err() {
        eprintln!("buttons: could not start");
    }
}

fn perform(action: Action, sender: &mpsc::Sender<Event>, mixer: &DeviceMixer) {
    let intent = match action {
        Action::Toggle => Intent::Toggle,
        Action::Next => Intent::Next,
        Action::Previous => Intent::Previous,
        Action::Louder | Action::Quieter => {
            let level = mixer.desired.load(Ordering::Acquire);
            mixer.set(if action == Action::Louder {
                level.saturating_add(STEP)
            } else {
                level.saturating_sub(STEP)
            });
            return;
        }
    };
    let _ = sender.try_send(Event { intent, received: tokio::time::Instant::now(), from_voice: false });
}

fn run(sender: mpsc::Sender<Event>, mixer: Arc<DeviceMixer>, active: Arc<AtomicBool>) {
    let Some((path, mut file)) = find_device() else {
        return eprintln!("buttons: no touch buttons found, Alice keeps them");
    };
    let fd = file.as_raw_fd();
    eprintln!("buttons: ready on {path}");
    let (mut held, mut grabbed, mut gesture) = (HashSet::<u16>::new(), false, Gesture::default());
    // `struct input_event` starts with a timeval whose fields are as wide as `long`.
    const EVENT: usize = 2 * size_of::<libc::c_long>() + 8;
    let mut buffer = [0u8; EVENT * 32];
    loop {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut poll, 1, 100) };
        loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => return eprintln!("buttons: input closed"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return eprintln!("buttons: input read failed: {e}"),
            };
            for event in buffer[..read - read % EVENT].chunks_exact(EVENT) {
                let at = 2 * size_of::<libc::c_long>();
                let kind = u16::from_ne_bytes([event[at], event[at + 1]]);
                let code = u16::from_ne_bytes([event[at + 2], event[at + 3]]);
                let value = i32::from_ne_bytes([event[at + 4], event[at + 5], event[at + 6], event[at + 7]]);
                if kind == EV_SYN && code == SYN_DROPPED {
                    held.clear();
                    gesture = Gesture::default();
                } else if kind == EV_KEY && value != 2 {
                    if value == 0 {
                        held.remove(&code);
                    } else {
                        held.insert(code);
                    }
                    if grabbed {
                        eprintln!("buttons: key {code} {}", if value == 0 { "up" } else { "down" });
                        if let Some(action) = gesture.key(code, value, Instant::now()) {
                            perform(action, &sender, &mixer);
                        }
                    }
                }
            }
        }
        if grabbed && let Some(action) = gesture.tick(Instant::now()) {
            perform(action, &sender, &mixer);
        }
        // Grabbing while a key is down would leave its release with Alice.
        let wanted = active.load(Ordering::Acquire);
        if wanted != grabbed && held.is_empty() {
            if unsafe { libc::ioctl(fd, EVIOCGRAB as _, libc::c_int::from(wanted)) } < 0 {
                return eprintln!("buttons: grab failed");
            }
            grabbed = wanted;
            gesture = Gesture::default();
            eprintln!("buttons: {}", if grabbed { "soundiya control" } else { "Alice control" });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_tap_on_the_alice_button_toggles_playback() {
        let (mut g, now) = (Gesture::default(), Instant::now());
        assert_eq!(g.key(ALICE, 1, now), None);
        assert_eq!(g.key(ALICE, 0, now), Some(Action::Toggle));
        // A release without a press (the key was down before the grab) does nothing.
        assert_eq!(g.key(ALICE, 0, now), None);
    }
    #[test]
    fn volume_buttons_repeat_while_held_and_stop_on_release() {
        let (mut g, now) = (Gesture::default(), Instant::now());
        assert_eq!(g.key(VOLUME_UP, 1, now), Some(Action::Louder));
        assert_eq!(g.tick(now + Duration::from_millis(100)), None);
        assert_eq!(g.tick(now + REPEAT_DELAY), Some(Action::Louder));
        assert_eq!(g.tick(now + REPEAT_DELAY + Duration::from_millis(50)), None);
        assert_eq!(g.tick(now + REPEAT_DELAY + REPEAT), Some(Action::Louder));
        assert_eq!(g.key(VOLUME_UP, 0, now), None);
        assert_eq!(g.tick(now + Duration::from_secs(5)), None);
        assert_eq!(g.key(VOLUME_DOWN, 1, now), Some(Action::Quieter));
    }
    #[test]
    fn a_volume_button_during_an_alice_hold_skips_and_swallows_the_toggle() {
        let (mut g, now) = (Gesture::default(), Instant::now());
        g.key(ALICE, 1, now);
        assert_eq!(g.key(VOLUME_UP, 1, now), Some(Action::Next));
        assert_eq!(g.tick(now + Duration::from_secs(5)), None);
        assert_eq!(g.key(VOLUME_DOWN, 1, now), Some(Action::Previous));
        assert_eq!(g.key(ALICE, 0, now), None);
    }
}
