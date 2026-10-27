//! The station's top (Alice) button. While soundiya has music loaded a tap on it pauses and
//! resumes, and Alice is told to stay quiet instead of announcing that Plus is missing.
//!
//! The input device is only watched, never grabbed: a grab would also hide the volume buttons
//! from the firmware, which then neither plays its volume sound nor lights the ring nor shows
//! the level on the display. With nothing loaded the button is left to Alice entirely.

use crate::voice::{Event, Intent};
use std::{
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
const EV_KEY: u16 = 1;
/// A touch sensor can report one tap as several presses.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// EVIOCGBIT(EV_KEY, 96): which keys the device can report.
const EVIOCGBIT_KEY: libc::c_ulong = 0x8060_4521;

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

/// Starts the button thread. `active` says whether music is loaded and the button is ours.
pub fn spawn(sender: mpsc::Sender<Event>, active: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("buttons".into())
        .spawn(move || run(sender, active));
    if spawned.is_err() {
        eprintln!("buttons: could not start");
    }
}

/// Reads `struct input_event`, whose leading timeval is as wide as `long`, and reports the
/// (code, value) of every key event.
fn keys(buffer: &[u8]) -> impl Iterator<Item = (u16, i32)> + '_ {
    const AT: usize = 2 * size_of::<libc::c_long>();
    buffer.chunks_exact(AT + 8).filter_map(|event| {
        let kind = u16::from_ne_bytes([event[AT], event[AT + 1]]);
        let code = u16::from_ne_bytes([event[AT + 2], event[AT + 3]]);
        let value = i32::from_ne_bytes([event[AT + 4], event[AT + 5], event[AT + 6], event[AT + 7]]);
        (kind == EV_KEY).then_some((code, value))
    })
}

fn run(sender: mpsc::Sender<Event>, active: Arc<AtomicBool>) {
    let Some((path, mut file)) = find_device() else {
        return eprintln!("buttons: no touch buttons found, Alice keeps them");
    };
    let fd = file.as_raw_fd();
    eprintln!("buttons: watching {path}");
    let mut buffer = [0u8; (2 * size_of::<libc::c_long>() + 8) * 32];
    let mut last_tap: Option<Instant> = None;
    loop {
        let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        unsafe { libc::poll(&mut poll, 1, 1000) };
        loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => return eprintln!("buttons: input closed"),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return eprintln!("buttons: input read failed: {e}"),
            };
            for (code, value) in keys(&buffer[..read]) {
                let recent = last_tap.is_some_and(|at| at.elapsed() < DEBOUNCE);
                if code == ALICE && value == 1 && !recent && active.load(Ordering::Acquire) {
                    last_tap = Some(Instant::now());
                    eprintln!("buttons: Alice button");
                    let event = Event {
                        intent: Intent::Toggle,
                        received: tokio::time::Instant::now(),
                        from_voice: false,
                    };
                    let _ = sender.try_send(event);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_key_events_and_skips_other_kinds() {
        let word = size_of::<libc::c_long>();
        let mut data = Vec::new();
        for (kind, code, value) in [(1u16, ALICE, 1i32), (0, 0, 0), (1, 115, 0)] {
            data.extend(vec![0u8; 2 * word]);
            data.extend(kind.to_ne_bytes());
            data.extend(code.to_ne_bytes());
            data.extend(value.to_ne_bytes());
        }
        assert_eq!(keys(&data).collect::<Vec<_>>(), [(ALICE, 1), (115, 0)]);
        // A partial trailing event is ignored.
        data.extend([0u8; 5]);
        assert_eq!(keys(&data).count(), 2);
    }
}
