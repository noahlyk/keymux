//! One keyboard's loop body: read a batch, give each key to the keymap at the moment the
//! keyboard reported it, send what comes out, and with nothing to read let the timeouts run
//! and wait. The keyboard and the virtual keyboard sit behind traits, so a test can script
//! the one and record the other.

use super::debounce::Debounce;
use super::output::{self, Pacer, Sink};
use super::KeymapProcessor;
use crate::keycode::KeyCode;
use evdev::{Device, EventType, InputEvent, Key};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, warn};

/// The physical keyboard: the device, or a script in a test.
pub trait Source {
    /// Read everything queued into `into`. `WouldBlock` when nothing is.
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()>;
    /// Nothing was queued: sleep until something is, `deadline` comes, or a short while
    /// passes, whichever is first.
    fn wait(&mut self, deadline: Option<Instant>);
}

impl Source for Device {
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()> {
        into.extend(self.fetch_events()?);
        Ok(())
    }

    fn wait(&mut self, deadline: Option<Instant>) {
        wait_for_input(self.as_raw_fd(), deadline);
    }
}

/// How a pass through the loop ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// A batch of events was handled
    Read,
    /// Nothing to read: the timeouts ran, and the keyboard was waited on
    Idle,
    /// Told to stop, with everything held released
    Shutdown,
    /// Alt+F12: the keyboard is to be given back, with everything held released
    Escaped,
}

/// How often the adaptive timing stats go to disk unasked.
const STATS_SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// The state of one keyboard's loop between passes.
pub struct Pump {
    name: String,
    user_id: u32,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    game_mode_rx: std::sync::mpsc::Receiver<bool>,
    save_stats_rx: std::sync::mpsc::Receiver<()>,
    alt_f12: AltF12,
    debounce: Debounce,
    pacer: Pacer,
    last_stats_save: Instant,
    events: Vec<InputEvent>,
}

impl Pump {
    pub fn new(
        name: String,
        user_id: u32,
        shutdown_rx: crossbeam_channel::Receiver<()>,
        game_mode_rx: std::sync::mpsc::Receiver<bool>,
        save_stats_rx: std::sync::mpsc::Receiver<()>,
    ) -> Self {
        Self {
            name,
            user_id,
            shutdown_rx,
            game_mode_rx,
            save_stats_rx,
            alt_f12: AltF12::default(),
            debounce: Debounce::default(),
            pacer: Pacer::default(),
            last_stats_save: Instant::now(),
            events: Vec::new(),
        }
    }

    /// One pass. The keymap is locked for the pass, so the device threads of one keyboard
    /// take turns and see one chord and layer state, and unlocked before any waiting.
    #[allow(clippy::significant_drop_tightening)] // the lock is meant to span the pass
    pub fn step<I: Source, O: Sink>(
        &mut self,
        shared: &Mutex<KeymapProcessor>,
        source: &mut I,
        sink: &mut O,
    ) -> io::Result<Step> {
        let mut keymap = crate::lock(shared);
        match self.shutdown_rx.try_recv() {
            Err(crossbeam_channel::TryRecvError::Empty) => {}
            // Asked to stop, or no one left to ask: the keyboard is given back either way
            signal => {
                warn!(
                    "{} for: {}",
                    if signal.is_ok() {
                        "Shutdown signal received"
                    } else {
                        "Shutdown channel disconnected"
                    },
                    self.name
                );
                let _ = keymap.save_adaptive_stats(self.user_id);
                output::release_all_keys(sink, &keymap);
                return Ok(Step::Shutdown);
            }
        }
        if let Ok(active) = self.game_mode_rx.try_recv() {
            info!(
                "Game mode {} for: {}",
                if active { "enabled" } else { "disabled" },
                self.name
            );
            keymap.set_game_mode(active);
        }
        // Adaptive timing stats go to disk on request, and every so often regardless
        if self.save_stats_rx.try_recv().is_ok()
            || self.last_stats_save.elapsed() >= STATS_SAVE_INTERVAL
        {
            let _ = keymap.save_adaptive_stats(self.user_id);
            self.last_stats_save = Instant::now();
        }

        self.events.clear();
        match source.read(&mut self.events) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // With no key to prompt them, timeouts resolve by themselves: a tap-dance
                // held past its term, a stuck stroke, a held stroke repeating
                let tap_dance = keymap.check_dt_timeouts();
                output::emit_result(sink, &mut self.pacer, tap_dance)?;
                let steno = keymap.check_steno_timeouts();
                output::emit_result(sink, &mut self.pacer, steno)?;
                let deadline = keymap.next_deadline();
                // Let the keyboard's other device threads in before waiting
                drop(keymap);
                source.wait(deadline);
                return Ok(Step::Idle);
            }
            Err(e) => return Err(e),
        }

        // The kernel stamps each event when it happened. A batch that waited while this
        // thread was busy or asleep is replayed at those moments, so the wait never looks
        // like a key held for longer than it was.
        let anchor = (Instant::now(), SystemTime::now());
        for ev in &self.events {
            // Alt+F12 releases this keyboard from keymux. It's checked on the raw key, before
            // the keymap, so no layer or remap can swallow it.
            if ev.event_type() == EventType::KEY
                && self.alt_f12.feed(Key::new(ev.code()), ev.value())
            {
                warn!("Alt+F12 pressed on {}; releasing it from keymux", self.name);
                let _ = keymap.save_adaptive_stats(self.user_id);
                output::release_all_keys(sink, &keymap);
                return Ok(Step::Escaped);
            }
            if ev.event_type() == EventType::KEY {
                if let Some(key) = KeyCode::from_evdev_code(ev.code()) {
                    // 1 is a press, 0 a release; 2, the kernel's own auto-repeat, is dropped:
                    // the keymap decides what a held key does
                    if ev.value() == 2 {
                        continue;
                    }
                    let pressed = ev.value() == 1;
                    let at = moment_of(anchor, ev);
                    // Switch-bounce chatter, and the matching half of its pair, never reaches
                    // the keymap - see debounce.rs. Judged by when the key really bounced.
                    if self.debounce.is_chatter(ev.code(), pressed, at) {
                        continue;
                    }
                    let result = keymap.process_key_at(key, pressed, at);
                    output::emit_result(sink, &mut self.pacer, result)?;
                    continue;
                }
            }
            // A key keymux doesn't know, or no key at all (SYN...): through unchanged
            sink.emit(std::slice::from_ref(ev))?;
        }
        Ok(Step::Read)
    }
}

/// When `event` happened, on the monotonic clock: the kernel stamped it on the system clock
/// as it happened, and `anchor` is both clocks read together once the batch was in hand.
fn moment_of(anchor: (Instant, SystemTime), event: &InputEvent) -> Instant {
    let age = anchor
        .1
        .duration_since(event.timestamp())
        .unwrap_or_default();
    anchor.0.checked_sub(age).unwrap_or(anchor.0)
}

/// How long the loop waits with no key and no timeout due before it looks at its channels
/// (shutdown, game mode, stats) again.
const IDLE_WAIT: Duration = Duration::from_millis(25);

/// Wait until the keyboard has an event to read, `deadline` comes, or `IDLE_WAIT` passes,
/// whichever is first. A key always wakes it at once.
fn wait_for_input(fd: RawFd, deadline: Option<Instant>) {
    let now = Instant::now();
    let wait = deadline.map_or(IDLE_WAIT, |at| {
        at.saturating_duration_since(now).min(IDLE_WAIT)
    });
    // poll counts whole milliseconds; round up so a deadline is never woken for early
    let millis = i32::try_from(wait.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX);
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pollfd` is one valid, initialized entry, and 1 says so
    unsafe { libc::poll(std::ptr::from_mut(&mut pollfd), 1, millis) };
}

/// Watches one keyboard's raw key events for Alt+F12, the chord that releases it from keymux.
/// It only counts as the very first key press after the keyboard is attached: the first press
/// decides, and anything other than Alt+F12 disarms the chord for good. Either Alt counts.
#[derive(Debug)]
struct AltF12 {
    left: bool,
    right: bool,
    armed: bool,
}

impl Default for AltF12 {
    fn default() -> Self {
        Self {
            left: false,
            right: false,
            armed: true,
        }
    }
}

impl AltF12 {
    /// Feed one raw key event, where `value` is 1 for press, 0 for release, 2 for repeat.
    /// True when Alt+F12 is the first key press: the keyboard should be released.
    fn feed(&mut self, key: Key, value: i32) -> bool {
        if key == Key::KEY_LEFTALT {
            self.left = value != 0;
            return false;
        }
        if key == Key::KEY_RIGHTALT {
            self.right = value != 0;
            return false;
        }
        // Releases and repeats aren't presses, so they don't decide anything
        if value != 1 {
            return false;
        }
        let escape = self.armed && key == Key::KEY_F12 && (self.left || self.right);
        self.armed = false;
        escape
    }
}

#[cfg(test)]
mod alt_f12_tests {
    use super::*;

    /// Whether the chord fires for these events, in order, starting from a freshly attached keyboard.
    fn fires(events: &[(Key, i32)]) -> bool {
        let mut chord = AltF12::default();
        events.iter().any(|&(key, value)| chord.feed(key, value))
    }

    #[test]
    fn left_alt_then_f12_fires() {
        assert!(fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn right_alt_then_f12_fires() {
        assert!(fires(&[(Key::KEY_RIGHTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn f12_alone_does_not_fire() {
        assert!(!fires(&[(Key::KEY_F12, 1)]));
    }

    #[test]
    fn alt_released_before_f12_does_not_fire() {
        assert!(!fires(&[
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_LEFTALT, 0),
            (Key::KEY_F12, 1)
        ]));
    }

    #[test]
    fn holding_one_alt_while_pressing_the_other_keeps_it_held() {
        // Left alt down, right alt down, left alt up: right alt still holds, so F12 fires
        assert!(fires(&[
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_RIGHTALT, 1),
            (Key::KEY_LEFTALT, 0),
            (Key::KEY_F12, 1),
        ]));
    }

    #[test]
    fn f12_repeat_and_release_do_not_fire() {
        assert!(!fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 2)]));
        assert!(!fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 0)]));
    }

    #[test]
    fn alt_f12_after_another_key_press_does_not_fire() {
        assert!(!fires(&[
            (Key::KEY_A, 1),
            (Key::KEY_A, 0),
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_F12, 1)
        ]));
    }

    #[test]
    fn alt_f12_after_a_modifier_press_does_not_fire() {
        // Ctrl+Alt+F12 is not the escape: Ctrl was the first press
        assert!(!fires(&[
            (Key::KEY_LEFTCTRL, 1),
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_F12, 1)
        ]));
    }

    #[test]
    fn alt_f12_after_a_long_gap_still_needs_to_be_first() {
        // Any press in between disarms it, even after the keyboard has been quiet
        assert!(!fires(&[
            (Key::KEY_LEFTSHIFT, 1),
            (Key::KEY_LEFTSHIFT, 0),
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_F12, 1)
        ]));
    }

    #[test]
    fn a_release_before_the_chord_does_not_disarm_it() {
        assert!(fires(&[
            (Key::KEY_A, 0),
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_F12, 1)
        ]));
    }
}
